// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Request-global state of the streams layer (`FG()`, `BG()`, `PG()`).
//!
//! PHP keeps these in per-request globals; here they are thread-local. The
//! VM configures them at request start ([`set_settings`], [`set_hooks`]) and
//! may call [`reset_request`] between requests.

use std::cell::RefCell;

use crate::stat::StatBuf;

/// INI settings consulted by the streams layer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent INI booleans
pub struct Settings {
    /// `include_path`.
    pub include_path: Vec<u8>,
    /// `open_basedir` (empty = unrestricted).
    pub open_basedir: Vec<u8>,
    /// `allow_url_fopen`.
    pub allow_url_fopen: bool,
    /// `allow_url_include`.
    pub allow_url_include: bool,
    /// `sys_temp_dir`.
    pub sys_temp_dir: Vec<u8>,
    /// `upload_tmp_dir`.
    pub upload_tmp_dir: Vec<u8>,
    /// `auto_detect_line_endings`.
    pub auto_detect_line_endings: bool,
    /// `default_socket_timeout` in seconds.
    pub default_socket_timeout: i64,
    /// `html_errors` (joins wrapper error lists with `<br />`).
    pub html_errors: bool,
    /// Whether the SAPI is the CLI (`php://stdin` semantics, `php://fd`).
    pub sapi_is_cli: bool,
    /// `user_agent` (`FG(user_agent)`; `None` when unset).
    pub user_agent: Option<Vec<u8>>,
    /// `from` (`FG(from_address)`; `None` when unset).
    pub from_address: Option<Vec<u8>>,
    /// `realpath_cache_size` in bytes (`CWDG(realpath_cache_size_limit)`;
    /// 0 disables the realpath cache).
    pub realpath_cache_size_limit: i64,
    /// `realpath_cache_ttl` in seconds.
    pub realpath_cache_ttl: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            include_path: b".:/usr/share/php".to_vec(),
            open_basedir: Vec::new(),
            allow_url_fopen: true,
            allow_url_include: false,
            sys_temp_dir: Vec::new(),
            upload_tmp_dir: Vec::new(),
            auto_detect_line_endings: false,
            default_socket_timeout: 60,
            html_errors: false,
            sapi_is_cli: true,
            user_agent: None,
            from_address: None,
            realpath_cache_size_limit: 4096 * 1024,
            realpath_cache_ttl: 120,
        }
    }
}

/// `PHPWRITE()` sink.
pub type OutputHook = Box<dyn FnMut(&[u8])>;
/// `zend_get_executed_filename()` provider.
pub type FilenameHook = Box<dyn Fn() -> Option<Vec<u8>>>;
/// Request body provider.
pub type BodyHook = Box<dyn FnMut() -> Vec<u8>>;

/// Callbacks into the VM / SAPI.
#[derive(Default)]
pub struct Hooks {
    /// `PHPWRITE()`: writes to the output layer (`php://output`).
    pub output: Option<OutputHook>,
    /// Returns the currently executing script file
    /// (`zend_get_executed_filename()`), used by include path resolution.
    pub executed_filename: Option<FilenameHook>,
    /// The request body read by `php://input`.
    pub request_body: Option<BodyHook>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("output", &self.output.is_some())
            .field("executed_filename", &self.executed_filename.is_some())
            .field("request_body", &self.request_body.is_some())
            .finish()
    }
}

#[derive(Debug, Default)]
struct StatCache {
    stat: Option<(Vec<u8>, StatBuf)>,
    lstat: Option<(Vec<u8>, StatBuf)>,
}

#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // independent request flags
struct Globals {
    settings: Settings,
    stat_cache: StatCache,
    cli_in: bool,
    cli_out: bool,
    cli_err: bool,
    user_stream_current_filename: Option<Vec<u8>>,
    in_user_include: bool,
    request_body: Option<Vec<u8>>,
    /// The umask before the request's first `umask()` call (`BG(umask)`),
    /// put back at request end.
    request_umask: Option<u32>,
}

thread_local! {
    static GLOBALS: RefCell<Globals> = RefCell::new(Globals::default());
    static HOOKS: RefCell<Hooks> = RefCell::new(Hooks::default());
}

/// Returns a copy of the current settings.
pub fn settings() -> Settings {
    GLOBALS.with(|g| g.borrow().settings.clone())
}

/// Replaces the settings.
pub fn set_settings(settings: Settings) {
    GLOBALS.with(|g| g.borrow_mut().settings = settings);
}

/// Modifies the settings in place.
pub fn update_settings(f: impl FnOnce(&mut Settings)) {
    GLOBALS.with(|g| f(&mut g.borrow_mut().settings));
}

/// Installs VM hooks.
pub fn set_hooks(hooks: Hooks) {
    HOOKS.with(|h| *h.borrow_mut() = hooks);
}

/// Writes to the output layer (`PHPWRITE`); falls back to stdout.
pub fn php_write(data: &[u8]) {
    let hook = HOOKS.with(|h| h.borrow_mut().output.take());
    if let Some(mut out) = hook {
        out(data);
        HOOKS.with(|h| {
            let mut h = h.borrow_mut();
            if h.output.is_none() {
                h.output = Some(out);
            }
        });
    } else {
        use std::io::Write;
        let _ = std::io::stdout().write_all(data);
    }
}

/// `zend_get_executed_filename()` via the hook.
pub fn executed_filename() -> Option<Vec<u8>> {
    HOOKS.with(|h| h.borrow().executed_filename.as_ref().and_then(|f| f()))
}

/// The request body for `php://input` (read once from the SAPI hook).
pub fn request_body() -> Vec<u8> {
    if let Some(b) = GLOBALS.with(|g| g.borrow().request_body.clone()) {
        return b;
    }
    let hook = HOOKS.with(|h| h.borrow_mut().request_body.take());
    let body = hook.map(|mut f| f()).unwrap_or_default();
    GLOBALS.with(|g| g.borrow_mut().request_body = Some(body.clone()));
    body
}

/// `php_clear_stat_cache()`: forgets the cached `stat`/`lstat` results.
pub fn clear_stat_cache() {
    GLOBALS.with(|g| {
        let mut g = g.borrow_mut();
        g.stat_cache.stat = None;
        g.stat_cache.lstat = None;
    });
}

/// Looks up the stat cache (`link` selects the lstat slot).
pub fn stat_cache_get(path: &[u8], link: bool) -> Option<StatBuf> {
    GLOBALS.with(|g| {
        let g = g.borrow();
        let slot = if link {
            &g.stat_cache.lstat
        } else {
            &g.stat_cache.stat
        };
        slot.as_ref().filter(|(p, _)| p == path).map(|(_, s)| *s)
    })
}

/// Stores a stat result in the cache (`link` selects the lstat slot).
pub fn stat_cache_put(path: &[u8], link: bool, sb: StatBuf) {
    GLOBALS.with(|g| {
        let mut g = g.borrow_mut();
        let slot = if link {
            &mut g.stat_cache.lstat
        } else {
            &mut g.stat_cache.stat
        };
        *slot = Some((path.to_vec(), sb));
    });
}

/// Marks a CLI standard stream as opened; returns whether it was already
/// opened before (`static int cli_in` & co. in `php_fopen_wrapper.c`).
pub(crate) fn cli_std_opened(which: u8) -> bool {
    GLOBALS.with(|g| {
        let mut g = g.borrow_mut();
        let slot = match which {
            0 => &mut g.cli_in,
            1 => &mut g.cli_out,
            _ => &mut g.cli_err,
        };
        std::mem::replace(slot, true)
    })
}

/// `FG(user_stream_current_filename)`.
pub(crate) fn user_stream_current_filename() -> Option<Vec<u8>> {
    GLOBALS.with(|g| g.borrow().user_stream_current_filename.clone())
}

/// Sets `FG(user_stream_current_filename)`.
pub(crate) fn set_user_stream_current_filename(v: Option<Vec<u8>>) {
    GLOBALS.with(|g| g.borrow_mut().user_stream_current_filename = v);
}

/// `PG(in_user_include)`.
pub fn in_user_include() -> bool {
    GLOBALS.with(|g| g.borrow().in_user_include)
}

/// Sets `PG(in_user_include)`, returning the old value.
pub fn set_in_user_include(v: bool) -> bool {
    GLOBALS.with(|g| std::mem::replace(&mut g.borrow_mut().in_user_include, v))
}

/// Records `old` as the umask to restore at request end, unless the
/// request changed it before (`BG(umask)` in `PHP_FUNCTION(umask)`).
pub fn note_request_umask(old: u32) {
    GLOBALS.with(|g| {
        g.borrow_mut().request_umask.get_or_insert(old);
    });
}

/// Resets per-request state (stat cache, request body, recursion guards,
/// volatile wrapper/filter registrations) and puts back the umask the
/// request found (`PHP_RSHUTDOWN(basic)`).
pub fn reset_request() {
    let umask = GLOBALS.with(|g| {
        let mut g = g.borrow_mut();
        g.stat_cache = StatCache::default();
        g.user_stream_current_filename = None;
        g.in_user_include = false;
        g.request_body = None;
        g.request_umask.take()
    });
    if let Some(mask) = umask {
        crate::sys::umask(mask);
    }
    crate::wrapper::reset_volatile();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_cache_slots() {
        clear_stat_cache();
        let sb = StatBuf {
            size: 5,
            ..StatBuf::default()
        };
        stat_cache_put(b"/a", false, sb);
        assert_eq!(stat_cache_get(b"/a", false).unwrap().size, 5);
        assert!(stat_cache_get(b"/a", true).is_none());
        assert!(stat_cache_get(b"/b", false).is_none());
        clear_stat_cache();
        assert!(stat_cache_get(b"/a", false).is_none());
    }

    #[test]
    fn settings_roundtrip() {
        update_settings(|s| s.allow_url_include = true);
        assert!(settings().allow_url_include);
        update_settings(|s| s.allow_url_include = false);
    }
}
