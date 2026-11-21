// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Per-request `ext/pcre` state (`PCRE_G`): the compiled-pattern cache, the
//! `pcre.*` INI settings and `preg_last_error()`.
//!
//! The VM is single-threaded, so the state lives in a thread-local
//! [`Pcre`]. It is only ever borrowed for the duration of a single
//! `php-pcre` call — never across a call back into the VM — so user
//! callbacks may freely re-enter the `preg_*` functions.

use std::cell::{Cell, RefCell};

use php_pcre::{MatchConfig, Pcre, PregError, Regex};
use php_runtime::ZStr;
use php_vm::ext::VmResult;
use php_vm::Vm;

thread_local! {
    static STATE: RefCell<Pcre> = RefCell::new(Pcre::new());
    /// Whether the `pcre.*` INI values must be re-read (set by the
    /// on-modify handlers and at request boundaries).
    static CONFIG_STALE: Cell<bool> = const { Cell::new(true) };
}

/// Whether `s` is known to be valid UTF-8: the string's
/// `IS_STR_VALID_UTF8` flag (`is_known_valid_utf8()`; the start-offset
/// check is php-pcre's).
pub(crate) fn known_valid_utf8(s: &ZStr) -> bool {
    s.is_valid_utf8()
}

/// Records that a `/u` match validated the whole subject
/// (`MatchOutcome::subject_valid_utf8`: no error, offset 0), as
/// `php_pcre_match_impl` adds `IS_STR_VALID_UTF8` to it; interned subjects
/// are left alone (`!ZSTR_IS_INTERNED`), which `mark_valid_utf8` does.
pub(crate) fn remember_valid_utf8(s: &ZStr) {
    s.mark_valid_utf8();
}

/// The match configuration for `subject`, carrying the UTF-8 validity hint.
pub(crate) fn config_for(subject: &ZStr) -> MatchConfig {
    with(|p| p.config()).with_known_valid_utf8(known_valid_utf8(subject))
}

/// Marks the cached `pcre.*` settings stale (INI on-modify).
pub(crate) fn invalidate_config() {
    CONFIG_STALE.with(|c| c.set(true));
}

/// Runs `f` with the request's [`Pcre`] state.
pub(crate) fn with<R>(f: impl FnOnce(&mut Pcre) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Stores the value `preg_last_error()` returns.
pub(crate) fn set_last_error(e: PregError) {
    with(|p| p.set_last_error(e));
}

/// The current `preg_last_error()` value.
pub(crate) fn last_error() -> PregError {
    with(|p| p.last_error())
}

/// `RINIT`/`RSHUTDOWN`: drops the cache and resets the error state.
pub(crate) fn reset() {
    with(|p| *p = Pcre::new());
    invalidate_config();
}

/// Re-reads the `pcre.*` INI settings when they changed and returns the
/// match configuration to use.
pub(crate) fn sync_config(vm: &Vm) -> MatchConfig {
    if !CONFIG_STALE.with(|c| c.replace(false)) {
        return with(|p| p.config());
    }
    let backtrack = vm.ini_long("pcre.backtrack_limit");
    let recursion = vm.ini_long("pcre.recursion_limit");
    let jit = vm.ini_bool("pcre.jit");
    with(|p| {
        p.set_backtrack_limit(backtrack);
        p.set_recursion_limit(recursion);
        p.set_jit(jit);
        p.config()
    })
}

/// The `LC_CTYPE` locale when it is not the C locale (`BG(ctype_string)`,
/// which the `setlocale()` binding publishes for the calling thread).
/// PCRE2 builds the tables from the thread's locale, which is that one.
/// The spellings of the C locale (`setlocale(LC_CTYPE, "C.UTF-8")`
/// included) mean "no locale-specific tables": the character tables of
/// `C.UTF-8` equal those of `C` for single bytes.
pub(crate) fn ctype_locale() -> Option<Vec<u8>> {
    php_support::fmt::locale::ctype_name().filter(|name| !is_c_locale(name))
}

/// Whether `name` names the C locale.
fn is_c_locale(name: &[u8]) -> bool {
    matches!(
        name,
        b"C" | b"POSIX" | b"C.UTF-8" | b"C.utf8" | b"C.UTF8" | b"C.utf-8"
    )
}

/// `pcre_get_compiled_regex_cache()`: compiles `pattern` (or fetches it
/// from the cache), emitting PHP's warnings. `Ok(None)` means the pattern
/// is invalid; the warning was emitted and `preg_last_error()` set.
pub(crate) fn compile(vm: &mut Vm, pattern: &[u8]) -> VmResult<Option<Regex>> {
    sync_config(vm);
    let locale = ctype_locale();
    let (result, warnings) = with(|p| {
        let r = p.compile_with_locale(pattern, locale.as_deref());
        (r, p.take_warnings())
    });
    for w in warnings {
        vm.php_warning(&String::from_utf8_lossy(&w))?;
    }
    match result {
        Ok(re) => Ok(Some(re)),
        Err(e) => {
            vm.php_warning(&String::from_utf8_lossy(&e.message()))?;
            // A user error handler may have run preg_* in between.
            set_last_error(e.last_error());
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_locale_spellings() {
        assert!(is_c_locale(b"C"));
        assert!(is_c_locale(b"C.UTF-8"));
        assert!(is_c_locale(b"POSIX"));
        assert!(!is_c_locale(b"de_DE.ISO-8859-1"));
    }

    /// The hint is the subject's own `IS_STR_VALID_UTF8` flag: any size,
    /// any number of subjects, shared by every handle, kept across
    /// requests' pcre state, never on interned strings.
    #[test]
    fn valid_utf8_hint_is_the_string_flag() {
        reset();
        let big = ZStr::from(vec![b'a'; 64 * 1024]);
        let small = ZStr::from("abc");
        let same_bytes = ZStr::from("abc");
        assert!(!known_valid_utf8(&big));
        remember_valid_utf8(&big);
        remember_valid_utf8(&small);
        assert!(known_valid_utf8(&big) && known_valid_utf8(&small));
        assert!(known_valid_utf8(&big.clone()));
        assert!(
            !known_valid_utf8(&same_bytes),
            "a flag, not a content cache"
        );
        assert_eq!(big.refcount(), 1, "no handle is kept");
        let interned = ZStr::intern(b"interned subject");
        remember_valid_utf8(&interned);
        assert!(!known_valid_utf8(&interned));
        reset();
        assert!(known_valid_utf8(&small));
        // A flagged string loses the flag when it changes in place.
        let mut grown = small;
        grown.append(b"\xff");
        assert!(!known_valid_utf8(&grown));
    }

    #[test]
    fn last_error_round_trip() {
        reset();
        assert_eq!(last_error(), PregError::NoError);
        set_last_error(PregError::BadUtf8);
        assert_eq!(last_error(), PregError::BadUtf8);
        reset();
        assert_eq!(last_error(), PregError::NoError);
    }
}
