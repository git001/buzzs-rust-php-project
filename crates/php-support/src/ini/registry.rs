// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The INI directive registry (`EG(ini_directives)` and `Zend/zend_ini.c`):
//! registration with defaults, `ini_get()`, `ini_set()`, `ini_restore()`,
//! `ini_get_all()`, request deactivation, and the typed modify handlers of
//! PHP's core directives.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::quantity::{escape, parse_bool, parse_quantity, parse_uquantity, quantity_warning};
use crate::diag::{Diagnostic, DiagnosticSink, ErrorLevel};
use crate::numeric::{atoi, atol};

/// Access levels (`PHP_INI_USER`, `PHP_INI_PERDIR`, `PHP_INI_SYSTEM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Access(pub u8);

impl Access {
    /// `PHP_INI_USER` — settable with `ini_set()`.
    pub const USER: Self = Self(1);
    /// `PHP_INI_PERDIR` — `.htaccess`/`.user.ini`.
    pub const PERDIR: Self = Self(2);
    /// `PHP_INI_SYSTEM` — `php.ini`/`-d`.
    pub const SYSTEM: Self = Self(4);
    /// `PHP_INI_ALL`.
    pub const ALL: Self = Self(7);

    /// Whether every bit of `other` is allowed … PHP checks `modifiable & type`.
    pub fn allows(self, modify_type: Self) -> bool {
        self.0 & modify_type.0 != 0
    }
}

/// Stages at which a value changes (`ZEND_INI_STAGE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Module startup (registration).
    Startup,
    /// Module shutdown.
    Shutdown,
    /// Request activation (per-dir / `-d` values).
    Activate,
    /// Request end (restoring modified values).
    Deactivate,
    /// `ini_set()` and friends.
    Runtime,
    /// `.htaccess`.
    Htaccess,
}

/// Typed value cached by a modify handler (what PHP stores in its globals).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum IniTyped {
    /// No typed value (plain string directive).
    #[default]
    None,
    /// Boolean directive.
    Bool(bool),
    /// Integer directive.
    Long(i64),
    /// Float directive.
    Double(f64),
}

/// The modify handler (`on_modify`) of a directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifyHandler {
    /// No handler: any value is accepted.
    None,
    /// `OnUpdateBool` (`zend_ini_parse_bool`).
    Bool,
    /// `OnUpdateLong` (`zend_ini_parse_quantity_warn`).
    Long,
    /// `OnUpdateLongGEZero`.
    LongGEZero,
    /// `OnUpdateLong` with a range check (opcache's `OnUpdateCounter`,
    /// `OnUpdateUnroll*`, `OnUpdateMaxTraceLength`): a value outside
    /// `min..=max` warns with `message` (`{name}` stands for the directive)
    /// and is rejected, so at startup the directive keeps its default.
    LongRange {
        /// Smallest accepted value.
        min: i64,
        /// Largest accepted value.
        max: i64,
        /// The warning, worded as in php-src.
        message: &'static str,
    },
    /// `OnUpdateReal` (`zend_strtod`).
    Real,
    /// `OnUpdateString` / `OnUpdateStr`.
    String,
    /// `OnUpdateStringUnempty` (rejects `""`).
    StringUnempty,
    /// `OnUpdateStrNotEmpty` (rejects `""`).
    StrNotEmpty,
    /// `OnSetPrecision` / `OnSetSerializePrecision` (`atol() >= -1`).
    Precision,
    /// `OnChangeMemoryLimit`.
    MemoryLimit,
    /// `OnChangeMaxMemoryLimit`.
    MaxMemoryLimit,
    /// `OnUpdateErrorReporting` (`atoi`, `E_ALL` when unset).
    ErrorReporting,
    /// `OnUpdateAssertions` (`zend.assertions`).
    Assertions,
    /// `OnUpdateDisplayErrors` (`on`/`stderr`/`stdout`/number).
    DisplayErrors,
    /// `OnUpdateTimeout` (`max_execution_time`, `atol`).
    Timeout,
    /// A handler that reads the number with `atoi()`/`atol()`: no
    /// quantity suffixes and no "Invalid quantity" warning (opcache's
    /// `OnUpdateMemoryConsumption`, `OnUpdateMaxAcceleratedFiles`).
    Atol,
    /// `OnSetExceptionStringParamMaxLen` (0..=1000000).
    ExceptionStringParamMaxLen,
    /// `OnSetLogFilter` (`all`/`no-ctrl`/`ascii`/`raw`).
    LogFilter,
    /// `OnUpdateFiberStackSize`.
    FiberStackSize,
    /// `OnUpdateGCEnabled` (`zend_ini_parse_bool`).
    GcEnabled,
    /// `OnUpdateMailCrLfMode` (`crlf`/`lf`/`mixed`/`os`).
    MailCrLfMode,
    /// `OnUpdateDefaultCharset` / `OnUpdateDefaultMimeTye`: a string that
    /// goes into the `Content-Type` header, so NUL, CR and LF are refused
    /// (bug #81518, header injection).
    HeaderValue,
    /// `OnUpdateAutoDetectLineEndings`: `OnUpdateBool` plus an
    /// "`auto_detect_line_endings` is deprecated" `E_DEPRECATED` for true values.
    AutoDetectLineEndings,
}

/// Static description of a directive (`zend_ini_entry_def`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniDirective {
    /// Directive name.
    pub name: String,
    /// Built-in default (`None` = `NULL`).
    pub default: Option<Vec<u8>>,
    /// Who may change it.
    pub access: Access,
    /// Modify handler.
    pub handler: ModifyHandler,
    /// Owning extension (`"Core"`, `"standard"`, …) for `ini_get_all()`.
    pub module: String,
}

impl IniDirective {
    /// Convenience constructor.
    pub fn new(
        name: &str,
        default: Option<&str>,
        access: Access,
        handler: ModifyHandler,
        module: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            default: default.map(|d| d.as_bytes().to_vec()),
            access,
            handler,
            module: module.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    def: IniDirective,
    value: Option<Vec<u8>>,
    orig_value: Option<Vec<u8>>,
    modifiable: Access,
    orig_modifiable: Access,
    modified: bool,
    typed: IniTyped,
}

/// One row of `ini_get_all()` with details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniEntryInfo {
    /// Directive name.
    pub name: String,
    /// `global_value` (value before runtime modification).
    pub global_value: Option<Vec<u8>>,
    /// `local_value` (current value).
    pub local_value: Option<Vec<u8>>,
    /// `builtin_default_value`.
    pub builtin_default_value: Option<Vec<u8>>,
    /// `access` bitmask.
    pub access: u8,
}

/// The registry of INI directives.
#[derive(Debug, Clone)]
pub struct IniRegistry {
    entries: HashMap<String, Entry>,
    order: Vec<String>,
    /// Configuration values (`php.ini`, `-d`) consulted at registration.
    config: HashMap<String, Vec<u8>>,
    modified: Vec<String>,
    modules: Vec<String>,
    /// Current memory usage reported by `memory_limit` failures
    /// (`zend_memory_usage(true)`; PHP's heap starts with one 2 MiB chunk).
    pub memory_usage: u64,
    /// Changes whenever a directive's value may have changed (see
    /// [`IniRegistry::generation`]).
    generation: u64,
}

/// Source of process-unique registry generations.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

impl Default for IniRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// `zend_binary_strcasecmp()` ordering.
fn strcasecmp(a: &str, b: &str) -> std::cmp::Ordering {
    let la = a.bytes().map(|c| c.to_ascii_lowercase());
    let lb = b.bytes().map(|c| c.to_ascii_lowercase());
    la.cmp(lb)
}

impl IniRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            config: HashMap::new(),
            modified: Vec::new(),
            modules: Vec::new(),
            memory_usage: 2 * 1024 * 1024,
            generation: next_generation(),
        }
    }

    /// A process-unique stamp of the current directive values: it changes
    /// on every operation that may change a value (registration, `alter`,
    /// restore, deactivation), and two distinct registries never share one
    /// unless one is an unmodified clone of the other. Callers that mirror
    /// INI values elsewhere re-read them only when it changes.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// A registry with PHP's core directives registered, applying
    /// `config` (`php.ini` / `-d name=value`) values first.
    pub fn with_core<I, K, V>(config: I, diag: &mut dyn DiagnosticSink) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<Vec<u8>>,
    {
        let mut r = Self::new();
        for (k, v) in config {
            r.set_config(k, v);
        }
        r.register_all(core_directives(), diag);
        r
    }

    /// Adds a configuration-hash value (`php.ini`/`-d`).  It takes effect
    /// for directives registered afterwards.
    pub fn set_config(&mut self, name: impl Into<String>, value: impl Into<Vec<u8>>) {
        self.config.insert(name.into(), value.into());
    }

    /// A configuration-hash value (`get_cfg_var()`, and the first source
    /// for `${name}` in INI files).
    pub fn config_value(&self, name: &str) -> Option<&[u8]> {
        self.config.get(name).map(Vec::as_slice)
    }

    /// Declares that an extension exists (for `ini_get_all($ext)`).
    pub fn register_module(&mut self, module: &str) {
        if !self.modules.iter().any(|m| m.eq_ignore_ascii_case(module)) {
            self.modules.push(module.to_string());
        }
    }

    /// Registers several directives.
    pub fn register_all(
        &mut self,
        defs: impl IntoIterator<Item = IniDirective>,
        diag: &mut dyn DiagnosticSink,
    ) {
        for d in defs {
            self.register(d, diag);
        }
    }

    /// `zend_register_ini_entries()` for one directive.  Returns `false`
    /// if a directive of that name already exists.
    pub fn register(&mut self, def: IniDirective, diag: &mut dyn DiagnosticSink) -> bool {
        if self.entries.contains_key(&def.name) {
            return false;
        }
        self.generation = next_generation();
        self.register_module(&def.module);
        let name = def.name.clone();
        let mut entry = Entry {
            modifiable: def.access,
            def,
            value: None,
            orig_value: None,
            orig_modifiable: Access(0),
            modified: false,
            typed: IniTyped::None,
        };
        let configured = self.config.get(&name).cloned();
        let mut done = false;
        if let Some(cv) = configured {
            if let Ok((typed, over)) = self.on_modify(&entry, Some(&cv), Stage::Startup, diag) {
                entry.typed = typed;
                entry.value = Some(over.unwrap_or(cv));
                done = true;
            }
        }
        if !done {
            entry.value.clone_from(&entry.def.default);
            if let Ok((typed, over)) =
                self.on_modify(&entry, entry.value.clone().as_deref(), Stage::Startup, diag)
            {
                entry.typed = typed;
                if over.is_some() {
                    entry.value = over;
                }
            }
        }
        self.order.push(name.clone());
        self.entries.insert(name, entry);
        true
    }

    /// A startup `on_modify` handler rejected the configured value of `name`
    /// (`FAILURE` at `ZEND_INI_STAGE_STARTUP`): the directive keeps its
    /// built-in default. Returns `false` for an unknown directive.
    pub fn reset_to_default(&mut self, name: &str) -> bool {
        let Some(e) = self.entries.get_mut(name) else {
            return false;
        };
        self.generation = next_generation();
        e.value.clone_from(&e.def.default);
        e.typed = IniTyped::None;
        true
    }

    /// Whether a directive is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// `zend_ini_get_value()` / `ini_get()`: `None` for unknown directives,
    /// `""` for registered directives without a value.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries
            .get(name)
            .map(|e| e.value.as_deref().unwrap_or_default())
    }

    /// The raw value (`None` when the directive has no value).
    pub fn raw(&self, name: &str) -> Option<&[u8]> {
        self.entries.get(name).and_then(|e| e.value.as_deref())
    }

    /// The value cached by the modify handler.
    pub fn typed(&self, name: &str) -> IniTyped {
        self.entries.get(name).map_or(IniTyped::None, |e| e.typed)
    }

    /// Integer view (`INI_INT()`-like: typed value or `atol()`).
    pub fn get_long(&self, name: &str) -> i64 {
        match self.typed(name) {
            IniTyped::Long(l) => l,
            IniTyped::Bool(b) => i64::from(b),
            #[allow(clippy::cast_possible_truncation)]
            IniTyped::Double(d) => d as i64,
            IniTyped::None => self.raw(name).map_or(0, atol),
        }
    }

    /// Boolean view (`INI_BOOL()`-like).
    pub fn get_bool(&self, name: &str) -> bool {
        match self.typed(name) {
            IniTyped::Bool(b) => b,
            IniTyped::Long(l) => l != 0,
            IniTyped::Double(d) => d != 0.0,
            IniTyped::None => self.raw(name).is_some_and(parse_bool),
        }
    }

    /// `zend_alter_ini_entry_ex()`.  Returns `false` on failure (unknown
    /// directive, insufficient access, or rejected by the handler).
    pub fn alter(
        &mut self,
        name: &str,
        new_value: &[u8],
        modify_type: Access,
        stage: Stage,
        force: bool,
        diag: &mut dyn DiagnosticSink,
    ) -> bool {
        let Some(entry) = self.entries.get(name) else {
            return false;
        };
        let modifiable = entry.modifiable;
        self.generation = next_generation();
        let modified = entry.modified;
        let mut entry = entry.clone();
        if stage == Stage::Activate && modify_type == Access::SYSTEM {
            entry.modifiable = Access::SYSTEM;
        }
        if !force && !entry.modifiable.allows(modify_type) {
            // The modifiable change above is kept, as in C.
            if let Some(e) = self.entries.get_mut(name) {
                e.modifiable = entry.modifiable;
            }
            return false;
        }
        if !modified {
            entry.orig_value.clone_from(&entry.value);
            entry.orig_modifiable = modifiable;
            entry.modified = true;
            self.modified.push(name.to_string());
        }
        let result = self.on_modify(&entry, Some(new_value), stage, diag);
        let ok = if let Ok((typed, over)) = result {
            entry.typed = typed;
            entry.value = Some(over.unwrap_or_else(|| new_value.to_vec()));
            true
        } else {
            false
        };
        self.entries.insert(name.to_string(), entry);
        ok
    }

    /// `ini_set($name, $value)`: returns the old value, or `None` (PHP's
    /// `false`) on failure.
    pub fn ini_set(
        &mut self,
        name: &str,
        value: &[u8],
        diag: &mut dyn DiagnosticSink,
    ) -> Option<Vec<u8>> {
        let old = self.get(name).map(<[u8]>::to_vec)?;
        if self.alter(name, value, Access::USER, Stage::Runtime, false, diag) {
            Some(old)
        } else {
            None
        }
    }

    /// `ini_get($name)`.
    pub fn ini_get(&self, name: &str) -> Option<Vec<u8>> {
        self.get(name).map(<[u8]>::to_vec)
    }

    fn restore_entry(&mut self, name: &str, stage: Stage, diag: &mut dyn DiagnosticSink) -> bool {
        let Some(entry) = self.entries.get(name).cloned() else {
            return false;
        };
        if !entry.modified {
            return true;
        }
        self.generation = next_generation();
        let result = self.on_modify(&entry, entry.orig_value.as_deref(), stage, diag);
        if stage == Stage::Runtime && result.is_err() {
            return false;
        }
        let mut e = entry;
        if let Ok((typed, _)) = result {
            e.typed = typed;
        }
        e.value = e.orig_value.take();
        e.modifiable = e.orig_modifiable;
        e.modified = false;
        e.orig_modifiable = Access(0);
        self.entries.insert(name.to_string(), e);
        self.modified.retain(|n| n != name);
        true
    }

    /// `zend_restore_ini_entry()` / `ini_restore()`.
    pub fn restore(&mut self, name: &str, stage: Stage, diag: &mut dyn DiagnosticSink) -> bool {
        match self.entries.get(name) {
            None => false,
            Some(e) if stage == Stage::Runtime && !e.modifiable.allows(Access::USER) => false,
            Some(_) => self.restore_entry(name, stage, diag),
        }
    }

    /// `ini_restore($name)`.
    pub fn ini_restore(&mut self, name: &str, diag: &mut dyn DiagnosticSink) {
        self.restore(name, Stage::Runtime, diag);
    }

    /// `zend_ini_deactivate()`: restores every modified directive at the
    /// end of the request.
    pub fn deactivate(&mut self, diag: &mut dyn DiagnosticSink) {
        self.generation = next_generation();
        for name in std::mem::take(&mut self.modified) {
            if let Some(e) = self.entries.get(&name) {
                let mut e = e.clone();
                if e.modified {
                    if let Ok((typed, _)) =
                        self.on_modify(&e, e.orig_value.as_deref(), Stage::Deactivate, diag)
                    {
                        e.typed = typed;
                    }
                    e.value = e.orig_value.take();
                    e.modifiable = e.orig_modifiable;
                    e.modified = false;
                    self.entries.insert(name, e);
                }
            }
        }
    }

    /// `set_include_path()`: returns the old value or `None` on failure.
    pub fn set_include_path(
        &mut self,
        path: &[u8],
        diag: &mut dyn DiagnosticSink,
    ) -> Option<Vec<u8>> {
        let old = self.raw("include_path").map(<[u8]>::to_vec);
        if self.alter(
            "include_path",
            path,
            Access::USER,
            Stage::Runtime,
            false,
            diag,
        ) {
            old
        } else {
            None
        }
    }

    /// `zend_ini_sort_entries()`: persistently sorts the shared directive
    /// table case-insensitively, including entries outside a queried module.
    pub fn sort_entries(&mut self) {
        self.order.sort_by(|a, b| strcasecmp(a, b));
    }

    /// `ini_get_all($extension, $details)` rows, sorted like PHP
    /// (case-insensitive).  `Err` carries the "Extension \"%s\" cannot be
    /// found" warning. The PHP binding calls [`Self::sort_entries`] first;
    /// this read-only query does not change the shared table order.
    pub fn ini_get_all(&self, extension: Option<&str>) -> Result<Vec<IniEntryInfo>, Diagnostic> {
        self.rows(extension, true)
    }

    /// The same rows as [`Self::ini_get_all`] but in current table order
    /// (registration order until [`Self::sort_entries`] is called),
    /// as `php --re`/`--ri` and `ReflectionExtension::getINIEntries()`
    /// list them (`zend_ini_directives` iteration order).
    pub fn ini_entries_registered(
        &self,
        extension: Option<&str>,
    ) -> Result<Vec<IniEntryInfo>, Diagnostic> {
        self.rows(extension, false)
    }

    fn rows(&self, extension: Option<&str>, sorted: bool) -> Result<Vec<IniEntryInfo>, Diagnostic> {
        if let Some(ext) = extension {
            if !self.modules.iter().any(|m| m.eq_ignore_ascii_case(ext)) {
                return Err(Diagnostic::docref(
                    ErrorLevel::WARNING,
                    format!("Extension \"{ext}\" cannot be found"),
                ));
            }
        }
        let mut names: Vec<&String> = self
            .order
            .iter()
            .filter(|n| {
                extension.is_none_or(|ext| self.entries[*n].def.module.eq_ignore_ascii_case(ext))
            })
            .collect();
        if sorted {
            names.sort_by(|a, b| strcasecmp(a, b));
        }
        Ok(names
            .into_iter()
            .map(|n| {
                let e = &self.entries[n];
                IniEntryInfo {
                    name: n.clone(),
                    global_value: if e.modified {
                        e.orig_value.clone()
                    } else {
                        e.value.clone()
                    },
                    local_value: e.value.clone(),
                    builtin_default_value: e.def.default.clone(),
                    access: e.modifiable.0,
                }
            })
            .collect())
    }

    /// Runs the modify handler.  `Ok((typed, override))` on success where
    /// `override` replaces the stored string (only `memory_limit` does that).
    #[allow(clippy::too_many_lines)]
    fn on_modify(
        &self,
        entry: &Entry,
        new_value: Option<&[u8]>,
        stage: Stage,
        diag: &mut dyn DiagnosticSink,
    ) -> Result<(IniTyped, Option<Vec<u8>>), ()> {
        let name = entry.def.name.as_str();
        let warn = |diag: &mut dyn DiagnosticSink, msg: String| {
            diag.report(Diagnostic::new(ErrorLevel::WARNING, msg));
        };
        let quantity = |diag: &mut dyn DiagnosticSink, v: &[u8]| {
            let (val, err) = parse_quantity(v);
            if let Some(e) = err {
                warn(diag, quantity_warning(&escape(name.as_bytes()), &e));
            }
            val
        };
        let v = new_value.unwrap_or_default();
        Ok(match entry.def.handler {
            ModifyHandler::None | ModifyHandler::String => (IniTyped::None, None),
            ModifyHandler::Bool | ModifyHandler::GcEnabled => (IniTyped::Bool(parse_bool(v)), None),
            ModifyHandler::AutoDetectLineEndings => {
                let on = parse_bool(v);
                if on && stage != Stage::Deactivate {
                    diag.report(Diagnostic::new(
                        ErrorLevel::DEPRECATED,
                        "auto_detect_line_endings is deprecated",
                    ));
                }
                (IniTyped::Bool(on), None)
            }
            ModifyHandler::Long => (IniTyped::Long(quantity(diag, v)), None),
            ModifyHandler::LongGEZero => {
                let t = quantity(diag, v);
                if t < 0 {
                    return Err(());
                }
                (IniTyped::Long(t), None)
            }
            ModifyHandler::LongRange { min, max, message } => {
                let t = quantity(diag, v);
                if !(min..=max).contains(&t) {
                    warn(diag, message.replace("{name}", name));
                    return Err(());
                }
                (IniTyped::Long(t), None)
            }
            ModifyHandler::Real => (IniTyped::Double(strtod_prefix(v)), None),
            ModifyHandler::StringUnempty => {
                if new_value.is_some_and(|n| n.first().is_none_or(|&c| c == 0)) {
                    return Err(());
                }
                (IniTyped::None, None)
            }
            ModifyHandler::StrNotEmpty => {
                if new_value.is_some_and(<[u8]>::is_empty) {
                    return Err(());
                }
                (IniTyped::None, None)
            }
            ModifyHandler::Precision => {
                let i = atol(v);
                if i < -1 {
                    return Err(());
                }
                (IniTyped::Long(i), None)
            }
            ModifyHandler::ErrorReporting => (
                IniTyped::Long(
                    new_value.map_or(i64::from(ErrorLevel::ALL.0), |n| i64::from(atoi(n))),
                ),
                None,
            ),
            ModifyHandler::Timeout | ModifyHandler::Atol => (IniTyped::Long(atol(v)), None),
            ModifyHandler::ExceptionStringParamMaxLen => {
                let i = atol(v);
                if !(0..=1_000_000).contains(&i) {
                    return Err(());
                }
                (IniTyped::Long(i), None)
            }
            ModifyHandler::DisplayErrors => (IniTyped::Long(display_errors_mode(new_value)), None),
            ModifyHandler::LogFilter => {
                if !matches!(v, b"all" | b"no-ctrl" | b"ascii" | b"raw") {
                    return Err(());
                }
                (IniTyped::None, None)
            }
            ModifyHandler::HeaderValue => {
                if v.iter().any(|&c| matches!(c, 0 | b'\r' | b'\n')) {
                    return Err(());
                }
                (IniTyped::None, None)
            }
            ModifyHandler::MailCrLfMode => {
                if !matches!(v, b"crlf" | b"lf" | b"mixed" | b"os") {
                    return Err(());
                }
                (IniTyped::None, None)
            }
            ModifyHandler::FiberStackSize => match new_value {
                Some(n) => {
                    let t = quantity(diag, n);
                    if t < 0 {
                        warn(diag, "fiber.stack_size must be a positive number".into());
                        return Err(());
                    }
                    (IniTyped::Long(t), None)
                }
                None => (IniTyped::None, None),
            },
            ModifyHandler::Assertions => {
                let val = quantity(diag, v);
                let cur = match entry.typed {
                    IniTyped::Long(l) => l,
                    _ => val,
                };
                if !matches!(stage, Stage::Startup | Stage::Shutdown)
                    && cur != val
                    && (cur < 0 || val < 0)
                {
                    warn(
                        diag,
                        "zend.assertions may be completely enabled or disabled only in php.ini"
                            .into(),
                    );
                    return Err(());
                }
                (IniTyped::Long(val), None)
            }
            ModifyHandler::MaxMemoryLimit => {
                let value = match new_value {
                    Some(n) => Self::uquantity(name, n, diag),
                    None => 1 << 30,
                };
                if value < self.memory_usage {
                    diag.report(Diagnostic::new(
                        ErrorLevel::ERROR,
                        format!(
                            "Failed to set memory limit to {} bytes (Current memory usage is {} bytes)",
                            to_i64(value),
                            self.memory_usage
                        ),
                    ));
                    return Err(());
                }
                (IniTyped::Long(to_i64(value)), None)
            }
            ModifyHandler::MemoryLimit => {
                let value = match new_value {
                    Some(n) => Self::uquantity(name, n, diag),
                    None => 1 << 30,
                };
                let max = match self.typed("max_memory_limit") {
                    IniTyped::Long(m) => to_u64(m),
                    _ => u64::MAX,
                };
                if value > max {
                    if value != u64::MAX {
                        warn(
                            diag,
                            format!(
                                "Failed to set memory_limit to {} bytes. Setting to max_memory_limit instead (currently: {} bytes)",
                                to_i64(value),
                                to_i64(max)
                            ),
                        );
                    }
                    let over = self.raw("max_memory_limit").map(<[u8]>::to_vec);
                    return Ok((IniTyped::Long(to_i64(max)), over));
                }
                if value < self.memory_usage && stage != Stage::Deactivate {
                    warn(
                        diag,
                        format!(
                            "Failed to set memory limit to {} bytes (Current memory usage is {} bytes)",
                            to_i64(value),
                            self.memory_usage
                        ),
                    );
                    return Err(());
                }
                (IniTyped::Long(to_i64(value)), None)
            }
        })
    }

    fn uquantity(name: &str, v: &[u8], diag: &mut dyn DiagnosticSink) -> u64 {
        let (val, err) = parse_uquantity(v);
        if let Some(e) = err {
            diag.report(Diagnostic::new(
                ErrorLevel::WARNING,
                quantity_warning(&escape(name.as_bytes()), &e),
            ));
        }
        val
    }
}

fn to_i64(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

fn to_u64(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

/// `php_get_display_errors_mode()`: 0 off, 1 stdout, 2 stderr.
fn display_errors_mode(value: Option<&[u8]>) -> i64 {
    let Some(v) = value else { return 1 };
    for (word, mode) in [
        (&b"on"[..], 1),
        (b"yes", 1),
        (b"true", 1),
        (b"stderr", 2),
        (b"stdout", 1),
    ] {
        if v.eq_ignore_ascii_case(word) {
            return mode;
        }
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mode = atol(v) as u8;
    if mode != 0 && mode != 1 && mode != 2 {
        1
    } else {
        i64::from(mode)
    }
}

/// `zend_strtod(s, NULL)`: the longest float prefix (after whitespace).
fn strtod_prefix(s: &[u8]) -> f64 {
    match crate::numeric::scan_numeric(s, true) {
        Some(r) => match r.value {
            #[allow(clippy::cast_precision_loss)]
            crate::numeric::NumericValue::Long(l) => l as f64,
            crate::numeric::NumericValue::Double(d) => d,
        },
        None => 0.0,
    }
}

/// PHP's core directives (`main/main.c`, `Zend/zend.c`, plus a few from
/// `ext/standard`) with their built-in defaults, access levels and
/// handlers.  The CLI SAPI additionally forces `display_errors=1`,
/// `html_errors=0`, `implicit_flush=1`, `output_buffering=0`,
/// `max_execution_time=0` and `max_input_time=-1` through the
/// configuration hash (see [`CLI_OVERRIDES`]).
#[allow(clippy::too_many_lines)]
pub fn core_directives() -> Vec<IniDirective> {
    use Access as A;
    use ModifyHandler as H;
    let pd_sys = Access(A::PERDIR.0 | A::SYSTEM.0);
    let c = |n: &str, d: Option<&str>, a: Access, h: ModifyHandler| {
        IniDirective::new(n, d, a, h, "Core")
    };
    let s = |n: &str, d: Option<&str>, a: Access, h: ModifyHandler| {
        IniDirective::new(n, d, a, h, "standard")
    };
    vec![
        c("highlight.comment", Some("#FF8000"), A::ALL, H::None),
        c("highlight.default", Some("#0000BB"), A::ALL, H::None),
        c("highlight.html", Some("#000000"), A::ALL, H::None),
        c("highlight.keyword", Some("#007700"), A::ALL, H::None),
        c("highlight.string", Some("#DD0000"), A::ALL, H::None),
        c("display_errors", Some("1"), A::ALL, H::DisplayErrors),
        c("display_startup_errors", Some("1"), A::ALL, H::Bool),
        c("enable_dl", Some("1"), A::SYSTEM, H::Bool),
        c("error_include_args", Some("0"), A::ALL, H::Bool),
        c("expose_php", Some("1"), A::SYSTEM, H::Bool),
        c("docref_root", Some(""), A::ALL, H::String),
        c("docref_ext", Some(""), A::ALL, H::String),
        c("html_errors", Some("1"), A::ALL, H::Bool),
        c("xmlrpc_errors", Some("0"), A::SYSTEM, H::Bool),
        c("xmlrpc_error_number", Some("0"), A::ALL, H::Long),
        c("max_input_time", Some("-1"), pd_sys, H::Long),
        c("ignore_user_abort", Some("0"), A::ALL, H::Bool),
        c("implicit_flush", Some("0"), A::ALL, H::Bool),
        c("log_errors", Some("0"), A::ALL, H::Bool),
        c("ignore_repeated_errors", Some("0"), A::ALL, H::Bool),
        c("ignore_repeated_source", Some("0"), A::ALL, H::Bool),
        c("report_memleaks", Some("1"), A::ALL, H::Bool),
        c("report_zend_debug", Some("0"), A::ALL, H::Bool),
        c("output_buffering", Some("0"), pd_sys, H::Long),
        c("output_handler", None, pd_sys, H::StrNotEmpty),
        c("register_argc_argv", Some("0"), pd_sys, H::Bool),
        c("auto_globals_jit", Some("1"), pd_sys, H::Bool),
        c("short_open_tag", Some("1"), pd_sys, H::Bool),
        c("unserialize_callback_func", None, A::ALL, H::String),
        c("serialize_precision", Some("-1"), A::ALL, H::Precision),
        c("arg_separator.output", Some("&"), A::ALL, H::StrNotEmpty),
        c("arg_separator.input", Some("&"), pd_sys, H::StrNotEmpty),
        c("auto_append_file", None, pd_sys, H::String),
        c("auto_prepend_file", None, pd_sys, H::String),
        c("doc_root", None, A::SYSTEM, H::StrNotEmpty),
        c("default_charset", Some("UTF-8"), A::ALL, H::HeaderValue),
        c(
            "default_mimetype",
            Some("text/html"),
            A::ALL,
            H::HeaderValue,
        ),
        c("internal_encoding", None, A::ALL, H::String),
        c("input_encoding", None, A::ALL, H::String),
        c("output_encoding", None, A::ALL, H::String),
        c("error_log", None, A::ALL, H::String),
        c("error_log_mode", Some("0644"), A::ALL, H::Long),
        c(
            "extension_dir",
            Some("/usr/lib/php/20230831"),
            A::SYSTEM,
            H::StringUnempty,
        ),
        c("sys_temp_dir", None, A::SYSTEM, H::StrNotEmpty),
        c(
            "include_path",
            Some(".:/usr/share/php"),
            A::ALL,
            H::StringUnempty,
        ),
        c("max_execution_time", Some("30"), A::ALL, H::Timeout),
        c("open_basedir", None, A::ALL, H::String),
        c("file_uploads", Some("1"), A::SYSTEM, H::Bool),
        c("upload_max_filesize", Some("2M"), pd_sys, H::Long),
        c("post_max_size", Some("8M"), pd_sys, H::Long),
        c("upload_tmp_dir", None, A::SYSTEM, H::StringUnempty),
        c("max_input_nesting_level", Some("64"), pd_sys, H::LongGEZero),
        c("max_input_vars", Some("1000"), pd_sys, H::LongGEZero),
        c("user_dir", None, A::SYSTEM, H::String),
        c("variables_order", Some("EGPCS"), pd_sys, H::StringUnempty),
        c("request_order", None, pd_sys, H::String),
        c("error_append_string", None, A::ALL, H::String),
        c("error_prepend_string", None, A::ALL, H::String),
        c("SMTP", Some("localhost"), A::ALL, H::None),
        c("smtp_port", Some("25"), A::ALL, H::None),
        c("mail.add_x_header", Some("0"), pd_sys, H::Bool),
        c("mail.mixed_lf_and_crlf", Some("0"), pd_sys, H::Bool),
        c("mail.cr_lf_mode", Some("crlf"), pd_sys, H::MailCrLfMode),
        c("mail.log", None, pd_sys, H::String),
        c("browscap", None, A::SYSTEM, H::None),
        c("max_memory_limit", Some("-1"), A::SYSTEM, H::MaxMemoryLimit),
        c("memory_limit", Some("128M"), A::ALL, H::MemoryLimit),
        c("memory_hard_limit_percent", None, A::ALL, H::None),
        c("precision", Some("14"), A::ALL, H::Precision),
        c("sendmail_from", None, A::ALL, H::None),
        c(
            "sendmail_path",
            Some("/usr/sbin/sendmail -t -i"),
            A::SYSTEM,
            H::None,
        ),
        c("mail.force_extra_parameters", None, pd_sys, H::None),
        c("disable_functions", Some(""), A::SYSTEM, H::None),
        c("max_file_uploads", Some("20"), pd_sys, H::None),
        c("max_multipart_body_parts", Some("-1"), pd_sys, H::None),
        c("allow_url_fopen", Some("1"), A::SYSTEM, H::Bool),
        c("allow_url_include", Some("0"), A::SYSTEM, H::Bool),
        c("enable_post_data_reading", Some("1"), pd_sys, H::Bool),
        c("realpath_cache_size", Some("4096K"), A::SYSTEM, H::Long),
        c("realpath_cache_ttl", Some("120"), A::SYSTEM, H::Long),
        c("user_ini.filename", Some(".user.ini"), A::SYSTEM, H::String),
        c("user_ini.cache_ttl", Some("300"), A::SYSTEM, H::Long),
        c("hard_timeout", Some("2"), A::SYSTEM, H::Long),
        c("syslog.facility", Some("LOG_USER"), A::SYSTEM, H::None),
        c("syslog.ident", Some("php"), A::SYSTEM, H::String),
        c("syslog.filter", Some("no-ctrl"), A::ALL, H::LogFilter),
        // Zend/zend.c
        c("error_reporting", None, A::ALL, H::ErrorReporting),
        c("fatal_error_backtraces", Some("1"), A::ALL, H::Bool),
        c("zend.assertions", Some("1"), A::ALL, H::Assertions),
        c("zend.enable_gc", Some("1"), A::ALL, H::GcEnabled),
        c("zend.multibyte", Some("0"), A::PERDIR, H::Bool),
        c("zend.script_encoding", None, A::ALL, H::None),
        c("zend.detect_unicode", Some("1"), A::ALL, H::Bool),
        c("zend.signal_check", Some("0"), A::SYSTEM, H::Bool),
        c("zend.exception_ignore_args", Some("0"), A::ALL, H::Bool),
        c(
            "zend.exception_string_param_max_len",
            Some("15"),
            A::ALL,
            H::ExceptionStringParamMaxLen,
        ),
        c("fiber.stack_size", None, A::ALL, H::FiberStackSize),
        c("zend.max_allowed_stack_size", Some("0"), A::SYSTEM, H::Long),
        c("zend.reserved_stack_size", Some("0"), A::SYSTEM, H::Long),
        // ext/standard (a selection used throughout the test suite)
        s("user_agent", None, A::ALL, H::String),
        s("from", None, A::ALL, H::String),
        s("default_socket_timeout", Some("60"), A::ALL, H::Long),
        s(
            "auto_detect_line_endings",
            Some("0"),
            A::ALL,
            H::AutoDetectLineEndings,
        ),
        s("unserialize_max_depth", Some("4096"), A::ALL, H::Long),
        s("url_rewriter.tags", Some("form="), A::ALL, H::String),
        s("url_rewriter.hosts", Some(""), A::ALL, H::String),
        s(
            "session.trans_sid_tags",
            Some("a=href,area=href,frame=src,form="),
            A::ALL,
            H::String,
        ),
    ]
}

/// Configuration values the CLI SAPI applies (`HARDCODED_INI` and
/// `sapi_cli_ini_defaults()` in `sapi/cli/php_cli.c`). php-src master no
/// longer hardcodes `register_argc_argv=1`: the CLI registers `$argv` and
/// `$argc` from `SG(request_info).argc` whatever its value.
pub const CLI_OVERRIDES: &[(&str, &str)] = &[
    ("display_errors", "1"),
    ("html_errors", "0"),
    ("implicit_flush", "1"),
    ("output_buffering", "0"),
    ("max_execution_time", "0"),
    ("max_input_time", "-1"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn core() -> IniRegistry {
        let mut d: Vec<Diagnostic> = Vec::new();
        let r = IniRegistry::with_core(CLI_OVERRIDES.iter().map(|&(k, v)| (k, v)), &mut d);
        assert!(d.is_empty(), "{d:?}");
        r
    }

    #[test]
    fn defaults() {
        let r = core();
        assert_eq!(r.get("precision"), Some(&b"14"[..]));
        assert_eq!(r.get_long("precision"), 14);
        assert_eq!(r.get("serialize_precision"), Some(&b"-1"[..]));
        assert_eq!(r.get_long("serialize_precision"), -1);
        assert_eq!(r.get("memory_limit"), Some(&b"128M"[..]));
        assert_eq!(r.get_long("memory_limit"), 128 << 20);
        assert_eq!(r.get("display_errors"), Some(&b"1"[..]));
        assert_eq!(r.get("html_errors"), Some(&b"0"[..]));
        assert_eq!(r.get("implicit_flush"), Some(&b"1"[..]));
        assert!(r.get_bool("implicit_flush"));
        assert_eq!(r.get("max_execution_time"), Some(&b"0"[..]));
        assert_eq!(r.get("include_path"), Some(&b".:/usr/share/php"[..]));
        assert_eq!(r.get("zend.assertions"), Some(&b"1"[..]));
        assert_eq!(r.get("short_open_tag"), Some(&b"1"[..]));
        assert_eq!(r.get("error_reporting"), Some(&b""[..]));
        assert_eq!(r.raw("error_reporting"), None);
        assert_eq!(r.get_long("error_reporting"), 30719);
        assert_eq!(r.get("nonexistent"), None);
        assert_eq!(r.get("output_handler"), Some(&b""[..]));
    }

    #[test]
    fn generation_changes_with_values() {
        let mut r = core();
        let mut d: Vec<Diagnostic> = Vec::new();
        let g0 = r.generation();
        assert_eq!(r.get("precision"), Some(&b"14"[..]));
        assert_eq!(r.generation(), g0, "reads keep the generation");
        assert_ne!(core().generation(), g0, "distinct registries differ");
        assert_eq!(r.clone().generation(), g0);
        r.ini_set("precision", b"6", &mut d);
        let g1 = r.generation();
        assert_ne!(g1, g0);
        r.ini_restore("precision", &mut d);
        let g2 = r.generation();
        assert_ne!(g2, g1);
        r.ini_set("include_path", b"/x", &mut d);
        assert_ne!(r.generation(), g2);
        let g3 = r.generation();
        r.deactivate(&mut d);
        assert_ne!(r.generation(), g3);
        assert_eq!(r.get("include_path"), Some(&b".:/usr/share/php"[..]));
    }

    #[test]
    fn ini_set_and_restore() {
        let mut r = core();
        let mut d: Vec<Diagnostic> = Vec::new();
        assert_eq!(r.ini_set("precision", b"6", &mut d), Some(b"14".to_vec()));
        assert_eq!(r.get("precision"), Some(&b"6"[..]));
        assert_eq!(r.get_long("precision"), 6);
        assert_eq!(r.ini_set("precision", b"-2", &mut d), None);
        assert_eq!(r.get("precision"), Some(&b"6"[..]));
        assert_eq!(r.ini_set("nope", b"1", &mut d), None);
        // PERDIR|SYSTEM directive is not user-settable.
        assert_eq!(r.ini_set("output_buffering", b"1", &mut d), None);
        assert_eq!(r.ini_set("html_errors", b"1", &mut d), Some(b"0".to_vec()));
        let all = r.ini_get_all(Some("core")).unwrap();
        let p = all.iter().find(|e| e.name == "precision").unwrap();
        assert_eq!(p.global_value.as_deref(), Some(&b"14"[..]));
        assert_eq!(p.local_value.as_deref(), Some(&b"6"[..]));
        assert_eq!(p.builtin_default_value.as_deref(), Some(&b"14"[..]));
        assert_eq!(p.access, 7);
        r.ini_restore("precision", &mut d);
        assert_eq!(r.get("precision"), Some(&b"14"[..]));
        assert_eq!(r.get_long("precision"), 14);
        r.deactivate(&mut d);
        assert_eq!(r.get("html_errors"), Some(&b"0"[..]));
        assert!(d.is_empty());
    }

    #[test]
    fn auto_detect_line_endings_deprecation() {
        let mut r = core();
        let mut d: Vec<Diagnostic> = Vec::new();
        assert_eq!(
            r.ini_set("auto_detect_line_endings", b"0", &mut d),
            Some(b"0".to_vec())
        );
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(
            r.ini_set("auto_detect_line_endings", b"on", &mut d),
            Some(b"0".to_vec())
        );
        assert_eq!(r.get("auto_detect_line_endings"), Some(&b"on"[..]));
        assert!(r.get_bool("auto_detect_line_endings"));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].level, ErrorLevel::DEPRECATED);
        assert_eq!(d[0].message, "auto_detect_line_endings is deprecated");
        assert!(!d[0].docref);
        let mut startup: Vec<Diagnostic> = Vec::new();
        let _ = IniRegistry::with_core([("auto_detect_line_endings", "1")], &mut startup);
        assert!(startup
            .iter()
            .any(|x| x.message == "auto_detect_line_endings is deprecated"));
    }

    #[test]
    fn memory_limit_warnings() {
        let mut r = core();
        let mut d: Vec<Diagnostic> = Vec::new();
        assert_eq!(r.ini_set("memory_limit", b"1M", &mut d), None);
        assert_eq!(
            d.pop().unwrap().message,
            "Failed to set memory limit to 1048576 bytes (Current memory usage is 2097152 bytes)"
        );
        assert_eq!(r.get("memory_limit"), Some(&b"128M"[..]));
        assert_eq!(
            r.ini_set("memory_limit", b"256M", &mut d),
            Some(b"128M".to_vec())
        );
        assert_eq!(
            r.ini_set("memory_limit", b"-1", &mut d),
            Some(b"256M".to_vec())
        );
        assert_eq!(r.ini_set("memory_limit", b"12abc", &mut d), None);
        assert_eq!(
            d[0].message,
            "Invalid \"memory_limit\" setting. Invalid quantity \"12abc\": unknown multiplier \"c\", interpreting as \"12\" for backwards compatibility"
        );
        assert!(d.iter().all(|x| !x.docref));
    }

    #[test]
    fn max_memory_limit() {
        let mut d: Vec<Diagnostic> = Vec::new();
        let mut r = IniRegistry::with_core([("max_memory_limit", "256M")], &mut d);
        assert!(d.is_empty());
        assert_eq!(
            r.ini_set("memory_limit", b"512M", &mut d),
            Some(b"128M".to_vec())
        );
        assert_eq!(
            d[0].message,
            "Failed to set memory_limit to 536870912 bytes. Setting to max_memory_limit instead (currently: 268435456 bytes)"
        );
        assert_eq!(r.get("memory_limit"), Some(&b"256M"[..]));
        d.clear();
        assert_eq!(
            r.ini_set("memory_limit", b"-1", &mut d),
            Some(b"256M".to_vec())
        );
        assert!(d.is_empty());
    }

    #[test]
    fn assertions_and_display_errors() {
        let mut r = core();
        let mut d: Vec<Diagnostic> = Vec::new();
        assert_eq!(r.ini_set("zend.assertions", b"-1", &mut d), None);
        assert_eq!(
            d[0].message,
            "zend.assertions may be completely enabled or disabled only in php.ini"
        );
        assert_eq!(
            r.ini_set("zend.assertions", b"0", &mut d),
            Some(b"1".to_vec())
        );
        assert_eq!(
            r.ini_set("display_errors", b"stderr", &mut d),
            Some(b"1".to_vec())
        );
        assert_eq!(r.get("display_errors"), Some(&b"stderr"[..]));
        assert_eq!(r.get_long("display_errors"), 2);
        r.ini_set("display_errors", b"off", &mut d);
        assert_eq!(r.get_long("display_errors"), 0);
        r.ini_set("display_errors", b"7", &mut d);
        assert_eq!(r.get_long("display_errors"), 1);
        let mut r2 = IniRegistry::with_core([("zend.assertions", "-1")], &mut d);
        assert_eq!(r2.ini_set("zend.assertions", b"1", &mut d), None);
    }

    #[test]
    fn other_handlers() {
        let mut r = core();
        let mut d: Vec<Diagnostic> = Vec::new();
        assert_eq!(r.ini_set("include_path", b"", &mut d), None);
        assert_eq!(
            r.set_include_path(b"/tmp", &mut d),
            Some(b".:/usr/share/php".to_vec())
        );
        assert_eq!(r.get("include_path"), Some(&b"/tmp"[..]));
        assert_eq!(r.ini_set("arg_separator.output", b"", &mut d), None);
        assert_eq!(
            r.ini_set("zend.exception_string_param_max_len", b"1000001", &mut d),
            None
        );
        assert_eq!(
            r.ini_set("zend.exception_string_param_max_len", b"20", &mut d),
            Some(b"15".to_vec())
        );
        assert_eq!(r.ini_set("syslog.filter", b"bogus", &mut d), None);
        assert_eq!(r.ini_set("fiber.stack_size", b"-1", &mut d), None);
        assert_eq!(
            d.last().unwrap().message,
            "fiber.stack_size must be a positive number"
        );
        assert_eq!(
            r.ini_set("error_reporting", b"32767", &mut d),
            Some(Vec::new())
        );
        assert_eq!(r.get_long("error_reporting"), 32767);
        assert_eq!(r.ini_set("docref_root", b"", &mut d), Some(Vec::new()));
        assert_eq!(r.ini_set("user_agent", b"3.14", &mut d), Some(Vec::new()));
        assert_eq!(r.get("user_agent"), Some(&b"3.14"[..]));
        assert_eq!(
            r.ini_set("default_socket_timeout", b"1x", &mut d),
            Some(b"60".to_vec())
        );
        assert!(d
            .last()
            .unwrap()
            .message
            .starts_with("Invalid \"default_socket_timeout\" setting."));
    }

    #[test]
    fn get_all_sorting_and_modules() {
        let r = core();
        let all = r.ini_get_all(None).unwrap();
        let names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_by(|a, b| strcasecmp(a, b));
        assert_eq!(names, sorted);
        // Registration order is kept by `ini_entries_registered` (same rows).
        let reg = r.ini_entries_registered(None).unwrap();
        let reg_names: Vec<&str> = reg.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            reg_names,
            r.order.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_ne!(reg_names, names);
        assert!(r.ini_entries_registered(Some("nosuchextension")).is_err());
        // "SMTP" sorts case-insensitively between "short_open_tag" and "smtp_port".
        let i = names.iter().position(|&n| n == "SMTP").unwrap();
        assert_eq!(names[i + 1], "smtp_port");
        let e = r.ini_get_all(Some("nosuchextension")).unwrap_err();
        assert_eq!(
            e.render(Some("ini_get_all")),
            "ini_get_all(): Extension \"nosuchextension\" cannot be found"
        );
        assert!(r.ini_get_all(Some("")).is_err());
        let st = r.ini_get_all(Some("standard")).unwrap();
        assert!(st
            .iter()
            .all(|e| ["user_agent", "from"].contains(&e.name.as_str())
                || e.name.contains('_')
                || e.name.contains('.')));
    }

    #[test]
    fn sorting_persists_across_module_queries_and_registration() {
        let mut r = IniRegistry::new();
        let mut d: Vec<Diagnostic> = Vec::new();
        for (name, module) in [("zeta", "first"), ("Beta", "second"), ("alpha", "first")] {
            assert!(r.register(
                IniDirective::new(name, Some("1"), Access::ALL, ModifyHandler::Long, module),
                &mut d,
            ));
        }
        assert_eq!(r.order, ["zeta", "Beta", "alpha"]);
        assert_eq!(r.ini_set("zeta", b"2", &mut d), Some(b"1".to_vec()));
        r.sort_entries();
        assert_eq!(r.order, ["alpha", "Beta", "zeta"]);
        let first = r.ini_entries_registered(Some("first")).unwrap();
        assert_eq!(first[0].name, "alpha");
        assert_eq!(first[1].name, "zeta");
        assert_eq!(first[1].global_value, Some(b"1".to_vec()));
        assert_eq!(first[1].local_value, Some(b"2".to_vec()));
        assert_eq!(r.get_long("zeta"), 2);
        assert!(r.ini_get_all(Some("missing")).is_err());
        assert_eq!(r.order, ["alpha", "Beta", "zeta"]);
        assert!(r.register(
            IniDirective::new("aardvark", None, Access::ALL, ModifyHandler::None, "second"),
            &mut d,
        ));
        assert_eq!(r.order, ["alpha", "Beta", "zeta", "aardvark"]);
        r.sort_entries();
        assert_eq!(r.order, ["aardvark", "alpha", "Beta", "zeta"]);
        r.ini_restore("zeta", &mut d);
        assert_eq!(r.get_long("zeta"), 1);
    }

    #[test]
    fn long_range_rejects_values_out_of_range() {
        const H: ModifyHandler = ModifyHandler::LongRange {
            min: 0,
            max: 255,
            message: "Invalid \"{name}\" setting. Should be between 0 and 255",
        };
        let mut d: Vec<Diagnostic> = Vec::new();
        let mut r = IniRegistry::new();
        r.set_config("x.counter", "300");
        r.set_config("x.quantity", "abc");
        r.register(IniDirective::new("x.counter", Some("8"), Access::ALL, H, "x"), &mut d);
        r.register(IniDirective::new("x.quantity", Some("8"), Access::ALL, H, "x"), &mut d);
        // At startup a value out of range keeps the default; "abc" is 0.
        assert_eq!(r.get("x.counter"), Some(&b"8"[..]));
        assert_eq!(r.get("x.quantity"), Some(&b"abc"[..]));
        assert_eq!(r.get_long("x.quantity"), 0);
        let msgs: Vec<&str> = d.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(msgs[0], "Invalid \"x.counter\" setting. Should be between 0 and 255");
        assert!(msgs[1].starts_with("Invalid \"x.quantity\" setting. Invalid quantity"));
        // ini_set() is refused with the same warning.
        d.clear();
        assert_eq!(r.ini_set("x.counter", b"-1", &mut d), None);
        assert_eq!(d.len(), 1);
        assert_eq!(r.ini_set("x.counter", b"255", &mut d), Some(b"8".to_vec()));
        assert_eq!(r.get_long("x.counter"), 255);
    }

    #[test]
    fn config_values_and_activation() {
        let mut d: Vec<Diagnostic> = Vec::new();
        let mut r =
            IniRegistry::with_core([("precision", "10"), ("output_buffering", "4096")], &mut d);
        assert_eq!(r.get("precision"), Some(&b"10"[..]));
        assert_eq!(r.get_long("output_buffering"), 4096);
        assert_eq!(r.config_value("precision"), Some(&b"10"[..]));
        // A rejected configured value falls back to the default.
        let r2 = IniRegistry::with_core([("precision", "-5")], &mut d);
        assert_eq!(r2.get("precision"), Some(&b"14"[..]));
        // Per-dir style activation.
        assert!(r.alter(
            "output_buffering",
            b"1",
            Access::PERDIR,
            Stage::Activate,
            false,
            &mut d
        ));
        assert!(!r.alter(
            "enable_dl",
            b"0",
            Access::PERDIR,
            Stage::Activate,
            false,
            &mut d
        ));
        assert!(r.alter(
            "enable_dl",
            b"0",
            Access::USER,
            Stage::Runtime,
            true,
            &mut d
        ));
        assert!(!r.register(
            IniDirective::new("precision", None, Access::ALL, ModifyHandler::None, "x"),
            &mut d
        ));
    }
}
