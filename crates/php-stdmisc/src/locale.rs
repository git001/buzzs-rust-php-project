// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `setlocale()`, `localeconv()`, `nl_langinfo()` (`ext/standard/string.c`)
//! over the C library, plus the startup/shutdown locale handling of
//! `main/main.c` (`zend_reset_lc_ctype_locale`).
//!
//! The locale is process-global state, exactly like in PHP.

use php_runtime::{ErrorLevel, RtResult, RuntimeContext, RuntimeError, Value, ZArray, ZStr};

use crate::sys;

/// `LC_*` and `nl_langinfo()` item constants (`ext/standard` names).
pub const CONSTANTS: &[(&str, i64)] = &[
    ("LC_CTYPE", libc::LC_CTYPE as i64),
    ("LC_NUMERIC", libc::LC_NUMERIC as i64),
    ("LC_TIME", libc::LC_TIME as i64),
    ("LC_COLLATE", libc::LC_COLLATE as i64),
    ("LC_MONETARY", libc::LC_MONETARY as i64),
    ("LC_ALL", libc::LC_ALL as i64),
    ("LC_MESSAGES", libc::LC_MESSAGES as i64),
    ("ABDAY_1", libc::ABDAY_1 as i64),
    ("ABDAY_2", libc::ABDAY_2 as i64),
    ("ABDAY_3", libc::ABDAY_3 as i64),
    ("ABDAY_4", libc::ABDAY_4 as i64),
    ("ABDAY_5", libc::ABDAY_5 as i64),
    ("ABDAY_6", libc::ABDAY_6 as i64),
    ("ABDAY_7", libc::ABDAY_7 as i64),
    ("DAY_1", libc::DAY_1 as i64),
    ("DAY_2", libc::DAY_2 as i64),
    ("DAY_3", libc::DAY_3 as i64),
    ("DAY_4", libc::DAY_4 as i64),
    ("DAY_5", libc::DAY_5 as i64),
    ("DAY_6", libc::DAY_6 as i64),
    ("DAY_7", libc::DAY_7 as i64),
    ("ABMON_1", libc::ABMON_1 as i64),
    ("ABMON_2", libc::ABMON_2 as i64),
    ("ABMON_3", libc::ABMON_3 as i64),
    ("ABMON_4", libc::ABMON_4 as i64),
    ("ABMON_5", libc::ABMON_5 as i64),
    ("ABMON_6", libc::ABMON_6 as i64),
    ("ABMON_7", libc::ABMON_7 as i64),
    ("ABMON_8", libc::ABMON_8 as i64),
    ("ABMON_9", libc::ABMON_9 as i64),
    ("ABMON_10", libc::ABMON_10 as i64),
    ("ABMON_11", libc::ABMON_11 as i64),
    ("ABMON_12", libc::ABMON_12 as i64),
    ("MON_1", libc::MON_1 as i64),
    ("MON_2", libc::MON_2 as i64),
    ("MON_3", libc::MON_3 as i64),
    ("MON_4", libc::MON_4 as i64),
    ("MON_5", libc::MON_5 as i64),
    ("MON_6", libc::MON_6 as i64),
    ("MON_7", libc::MON_7 as i64),
    ("MON_8", libc::MON_8 as i64),
    ("MON_9", libc::MON_9 as i64),
    ("MON_10", libc::MON_10 as i64),
    ("MON_11", libc::MON_11 as i64),
    ("MON_12", libc::MON_12 as i64),
    ("AM_STR", libc::AM_STR as i64),
    ("PM_STR", libc::PM_STR as i64),
    ("D_T_FMT", libc::D_T_FMT as i64),
    ("D_FMT", libc::D_FMT as i64),
    ("T_FMT", libc::T_FMT as i64),
    ("T_FMT_AMPM", libc::T_FMT_AMPM as i64),
    ("ERA", libc::ERA as i64),
    ("ERA_YEAR", 0x2_0000 + 45),
    ("ERA_D_T_FMT", libc::ERA_D_T_FMT as i64),
    ("ERA_D_FMT", libc::ERA_D_FMT as i64),
    ("ERA_T_FMT", libc::ERA_T_FMT as i64),
    ("ALT_DIGITS", libc::ALT_DIGITS as i64),
    ("INT_CURR_SYMBOL", 0x4_0000),
    ("CURRENCY_SYMBOL", 0x4_0001),
    ("CRNCYSTR", libc::CRNCYSTR as i64),
    ("MON_DECIMAL_POINT", 0x4_0002),
    ("MON_THOUSANDS_SEP", 0x4_0003),
    ("MON_GROUPING", 0x4_0004),
    ("POSITIVE_SIGN", 0x4_0005),
    ("NEGATIVE_SIGN", 0x4_0006),
    ("INT_FRAC_DIGITS", 0x4_0007),
    ("FRAC_DIGITS", 0x4_0008),
    ("P_CS_PRECEDES", 0x4_0009),
    ("P_SEP_BY_SPACE", 0x4_000a),
    ("N_CS_PRECEDES", 0x4_000b),
    ("N_SEP_BY_SPACE", 0x4_000c),
    ("P_SIGN_POSN", 0x4_000d),
    ("N_SIGN_POSN", 0x4_000e),
    ("DECIMAL_POINT", 0x1_0000),
    ("RADIXCHAR", libc::RADIXCHAR as i64),
    ("THOUSANDS_SEP", 0x1_0001),
    ("THOUSEP", libc::THOUSEP as i64),
    ("GROUPING", 0x1_0002),
    ("YESEXPR", libc::YESEXPR as i64),
    ("NOEXPR", libc::NOEXPR as i64),
    ("YESSTR", 0x5_0002),
    ("NOSTR", 0x5_0003),
    ("CODESET", libc::CODESET as i64),
];

/// Per-request locale state (`BG(locale_changed)`, `BG(ctype_string)`).
#[derive(Debug, Default, Clone)]
pub struct LocaleState {
    /// Whether a script changed the locale (restored at request end).
    pub locale_changed: bool,
    /// The `LC_CTYPE` locale name set by the script (`None` = "C").
    pub ctype_string: Option<ZStr>,
}

/// `zend_reset_lc_ctype_locale()`: the process's `LC_CTYPE` = "C.UTF-8"
/// (or "C"), which PHP does at startup so that multibyte-aware functions
/// such as `escapeshellarg()` see UTF-8. Every thread starts from it.
pub fn reset_lc_ctype() {
    if !sys::set_process_locale(libc::LC_CTYPE, b"C.UTF-8") {
        let _ = sys::set_process_locale(libc::LC_CTYPE, b"C");
    }
}

impl LocaleState {
    /// Request shutdown (`basic_globals` RSHUTDOWN): restores "C" (and
    /// the startup `LC_CTYPE`) if the script changed the locale: the
    /// thread returns to the process's locale, which is that.
    pub fn request_shutdown(&mut self) {
        if self.locale_changed {
            sys::reset_thread_locale();
            self.ctype_string = None;
            self.locale_changed = false;
        }
    }

    /// `try_setlocale_str()`.
    fn try_str(
        &mut self,
        cat: i64,
        loc: &[u8],
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> RtResult<Option<ZStr>> {
        let query = loc == b"0";
        if !query && loc.len() >= 255 {
            ctx.diagnostic(
                ErrorLevel::Warning,
                "setlocale(): Specified locale name is too long",
            )?;
            return Ok(None);
        }
        let Ok(cat) = i32::try_from(cat) else {
            return Ok(None);
        };
        let Some(retval) = sys::setlocale(cat, (!query).then_some(loc)) else {
            return Ok(None);
        };
        if !query {
            self.locale_changed = true;
            if cat == libc::LC_CTYPE || cat == libc::LC_ALL {
                if retval == b"C" {
                    self.ctype_string = None;
                    return Ok(Some(ZStr::from("C")));
                }
                let s = if retval == loc {
                    ZStr::new(loc)
                } else {
                    ZStr::new(retval)
                };
                self.ctype_string = Some(s.clone());
                return Ok(Some(s));
            }
            if retval == loc {
                return Ok(Some(ZStr::new(loc)));
            }
        }
        Ok(Some(ZStr::new(retval)))
    }

    /// `try_setlocale_zval()` for array elements.
    fn try_value(
        &mut self,
        cat: i64,
        v: &Value,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> RtResult<Option<ZStr>> {
        let s = v.to_zstr(ctx)?;
        if s.as_bytes().contains(&0) {
            return Err(RuntimeError::ValueError(
                "setlocale(): Argument #2 ($locales) must not contain any null bytes".into(),
            ));
        }
        self.try_str(cat, s.as_bytes(), ctx)
    }

    /// `setlocale($category, $locales, ...$rest)`: `args` are the
    /// `$locales` and `$rest` arguments as passed. Returns the new locale
    /// name or `false`.
    pub fn setlocale(
        &mut self,
        category: i64,
        args: &[Value],
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> RtResult<Value> {
        let arg_label = |i: usize| {
            if i == 0 {
                "#2 ($locales)".to_string()
            } else {
                format!("#{} ($rest)", i + 2)
            }
        };
        // Parameter validation (zend_parse_arg_path_str semantics).
        let mut strings: Vec<Option<ZStr>> = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let a = a.deref();
            match &a {
                Value::Array(_) => {
                    if i != 0 {
                        return Err(RuntimeError::TypeError(format!(
                            "setlocale(): Argument {} must be of type ?string, array given",
                            arg_label(i)
                        )));
                    }
                    if args.len() > 1 {
                        return Err(RuntimeError::ArgumentCountError(format!(
                            "setlocale() expects exactly 2 arguments when argument #2 ($locales) is an array, {} given",
                            args.len() + 1
                        )));
                    }
                    strings.push(None);
                    break;
                }
                Value::Null => strings.push(None),
                Value::Str(s) => {
                    if s.as_bytes().contains(&0) {
                        return Err(RuntimeError::ValueError(format!(
                            "setlocale(): Argument {} must not contain any null bytes",
                            arg_label(i)
                        )));
                    }
                    strings.push(Some(s.clone()));
                }
                Value::Int(_) | Value::Float(_) | Value::Bool(_) => {
                    strings.push(Some(a.to_zstr(ctx)?));
                }
                Value::Object(_) => strings.push(Some(a.to_zstr(ctx)?)),
                other => {
                    let expected = if i == 0 {
                        "array|string|null"
                    } else {
                        "?string"
                    };
                    return Err(RuntimeError::TypeError(format!(
                        "setlocale(): Argument {} must be of type {expected}, {} given",
                        arg_label(i),
                        other.type_name()
                    )));
                }
            }
        }
        for (i, a) in args.iter().enumerate() {
            let result = match a.deref() {
                Value::Array(arr) => {
                    for elem in arr.values() {
                        if let Some(r) = self.try_value(category, elem, ctx)? {
                            return Ok(Value::Str(r));
                        }
                    }
                    continue;
                }
                Value::Null => self.try_str(category, b"", ctx)?,
                _ => {
                    let s = strings[i].clone().unwrap_or_default();
                    self.try_str(category, s.as_bytes(), ctx)?
                }
            };
            if let Some(r) = result {
                return Ok(Value::Str(r));
            }
        }
        Ok(Value::Bool(false))
    }
}

/// `localeconv()`.
#[must_use]
pub fn localeconv() -> ZArray {
    let l = sys::localeconv();
    let mut out = ZArray::new();
    let t = out.make_mut();
    for (k, v) in l.strings {
        t.insert(k, Value::Str(ZStr::new(v)));
    }
    for (k, v) in l.chars {
        t.insert(k, Value::Int(i64::from(v)));
    }
    let group = |g: Vec<i8>| ZArray::from_values(g.into_iter().map(|b| Value::Int(i64::from(b))));
    t.insert("grouping", Value::Array(group(l.grouping)));
    t.insert("mon_grouping", Value::Array(group(l.mon_grouping)));
    out
}

/// `localeconv_decimal_point()` (`ext/standard/string.c`): the first byte of
/// the current `localeconv()->decimal_point` (NUL when it is empty), which
/// PHP's `%f`/`%g` formatting uses (`LCONV_DECIMAL_POINT`).
#[must_use]
pub fn decimal_point() -> u8 {
    sys::localeconv()
        .strings
        .iter()
        .find(|(k, _)| *k == "decimal_point")
        .map_or(b'.', |(_, v)| v.first().copied().unwrap_or(0))
}

/// `nl_langinfo($item)`: `false` (with a warning) for items PHP does not
/// accept.
pub fn nl_langinfo(item: i64, ctx: &mut (impl RuntimeContext + ?Sized)) -> RtResult<Value> {
    let valid = CONSTANTS
        .iter()
        .any(|(name, v)| *v == item && !name.starts_with("LC_"));
    let Some(item32) = i32::try_from(item).ok().filter(|_| valid) else {
        ctx.diagnostic(
            ErrorLevel::Warning,
            &format!("nl_langinfo(): Item '{item}' is not valid"),
        )?;
        return Ok(Value::Bool(false));
    };
    Ok(match sys::nl_langinfo(item32) {
        Some(v) => Value::Str(ZStr::new(v)),
        None => Value::Bool(false),
    })
}
