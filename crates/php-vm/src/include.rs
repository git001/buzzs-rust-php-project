// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Compiling and running files: the main script, `include`/`require`
//! and `eval`.

use crate::compiler::{self, CompileOptions, CompiledUnit, CtConstants};
use crate::opcode::IncludeKind;
use crate::vm::{Frame, FrameKind, RetTarget, Throw, Vm, VmResult};
use php_runtime::{Value, ZStr};
use std::rc::Rc;

/// Outcome of compiling source code.
#[derive(Clone, Debug)]
pub enum CompileOutcome {
    Ok(Rc<CompiledUnit>),
    /// A `ParseError` (or thrown `CompileError`) exception was created.
    ParseError(php_runtime::Obj),
    /// A fatal error was reported.
    Fatal,
}

/// A hook called when compiling file source (`zend_compile_file` override point).
pub type CompileHookFn = fn(&mut Vm, Rc<[u8]>, &ZStr, bool) -> VmResult<CompileOutcome>;

/// A hook that serves an `include` whose file cannot be opened from a
/// script cache (opcache finds a script by its key before opening the
/// file, so with `opcache.validate_timestamps=0` a deleted file still
/// runs). Returns the cached unit and its path.
pub type CachedIncludeHookFn = fn(&mut Vm, &[u8]) -> VmResult<Option<(ZStr, Rc<CompiledUnit>)>>;

/// A hook asked for a cached unit of an included file before the file is
/// read (by its opened path): a script cache serves the include without
/// reading the source, as opcache loads a script before
/// `zend_stream_fixup` reads it. `None` falls back to reading and
/// compiling.
pub type PrecompiledHookFn = fn(&mut Vm, &ZStr) -> VmResult<Option<Rc<CompiledUnit>>>;

/// `tsrm_realpath()` as the stream layer implements it, with
/// `CWDG(realpath_cache)`: the real path of an existing file, `None` when
/// it cannot be resolved.
pub type IncludeRealpathFn = fn(&[u8]) -> Option<Vec<u8>>;

/// A pass registered with `zend_optimizer_register_pass`, run on each
/// script opcache optimizes.
pub type OptimizerPassFn = fn(&mut Vm);

thread_local! {
    static OPTIMIZER_PASSES: std::cell::RefCell<Vec<OptimizerPassFn>> = const { std::cell::RefCell::new(Vec::new()) };
    static CACHED_INCLUDE_HOOK: std::cell::Cell<Option<CachedIncludeHookFn>> = const { std::cell::Cell::new(None) };
    static COMPILE_HOOK: std::cell::Cell<Option<CompileHookFn>> = const { std::cell::Cell::new(None) };
    static PRECOMPILED_HOOK: std::cell::Cell<Option<PrecompiledHookFn>> = const { std::cell::Cell::new(None) };
    static REALPATH_HOOK: std::cell::Cell<Option<IncludeRealpathFn>> = const { std::cell::Cell::new(None) };
    /// Set while compiling a file opened through a stream wrapper other
    /// than `file://` and `phar://`: such source bypasses the compile hook
    /// (opcache's `persistent_compile_file` falls back to the original
    /// `compile_file` for it, bug #64482).
    static UNCACHEABLE_STREAM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// `ZEND_COMPILE_IGNORE_OTHER_FILES`: while set, the compiler does not
    /// see user functions declared by other files, so the code it produces
    /// does not depend on what happened to be declared before (opcache sets
    /// it for every script it caches; ZendAccelerator.c).
    static IGNORE_OTHER_FILES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set while opcache compiles a script it caches under
    /// `opcache.save_comments=0`: the doc comments are dropped.
    static STRIP_DOC_COMMENTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `is_stream_path() && !is_cacheable_stream_path()` (ZendAccelerator.c):
/// a `scheme://` name whose wrapper is neither `file` nor `phar`.
fn is_uncacheable_stream_path(name: &[u8]) -> bool {
    let scheme = name
        .iter()
        .take_while(|&&c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        .count();
    scheme > 0
        && name[scheme..].starts_with(b"://")
        && !name.starts_with(b"file://")
        && !name.starts_with(b"phar://")
}

impl Vm {
    /// Installs a compile hook (`zend_compile_file` override point) for files.
    pub fn set_compile_hook(f: CompileHookFn) {
        COMPILE_HOOK.with(|h| h.set(Some(f)));
    }

    /// Installs the hook asked for a cached unit before an included file
    /// is read ([`PrecompiledHookFn`]).
    pub fn set_precompiled_hook(f: PrecompiledHookFn) {
        PRECOMPILED_HOOK.with(|h| h.set(Some(f)));
    }

    /// `zend_optimizer_register_pass`: `pass` runs after opcache's
    /// optimizer on each script it compiles.
    pub fn register_optimizer_pass(pass: OptimizerPassFn) {
        OPTIMIZER_PASSES.with(|p| p.borrow_mut().push(pass));
    }

    /// `zend_optimizer_call_registered_passes`, at the end of
    /// `zend_optimize_script`.
    pub fn call_registered_optimizer_passes(&mut self) {
        let passes = OPTIMIZER_PASSES.with(|p| p.borrow().clone());
        for pass in passes {
            pass(self);
        }
    }

    /// Installs the resolver of included files' opened paths
    /// (`tsrm_realpath()` over the realpath cache). Without one, each
    /// include resolves its path with `realpath(3)`, a `readlink` per
    /// path component.
    pub fn set_include_realpath_hook(f: IncludeRealpathFn) {
        REALPATH_HOOK.with(|h| h.set(Some(f)));
    }

    /// Installs the hook serving includes of unopenable files from a
    /// script cache.
    pub fn set_cached_include_hook(f: CachedIncludeHookFn) {
        CACHED_INCLUDE_HOOK.with(|h| h.set(Some(f)));
    }

    /// Runs `f` with `ZEND_COMPILE_IGNORE_OTHER_FILES` set: compiling then
    /// ignores user functions declared by other files, so the result can
    /// be cached and reused whatever else is declared.
    pub fn with_ignore_other_files<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = IGNORE_OTHER_FILES.with(|c| c.replace(true));
        let out = f(self);
        IGNORE_OTHER_FILES.with(|c| c.set(prev));
        out
    }

    /// Runs `f`, compiling files without their doc comments when `strip`
    /// is set: opcache's persisting of a script under
    /// `opcache.save_comments=0` (`zend_persist`).
    pub fn with_doc_comments_stripped<R>(
        &mut self,
        strip: bool,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let prev = STRIP_DOC_COMMENTS.with(|c| c.replace(strip));
        let out = f(self);
        STRIP_DOC_COMMENTS.with(|c| c.set(prev));
        out
    }

    /// A hash of every compiler input besides the source that changes the
    /// code a file compiles to: options from INI (`zend.assertions`, the
    /// optimization level), which builtins compile to dedicated opcodes,
    /// and the constants substituted at compile time. A cached unit is
    /// reused only under the same fingerprint. `None` when the current
    /// setup must not use cached code at all (user opcode handlers are
    /// installed, or `zend.multibyte` keeps the AST for diagnostics).
    pub fn compile_fingerprint(&mut self) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        if !self.user_opcodes().is_empty() || self.ini.get_bool("zend.multibyte") {
            return None;
        }
        let builtin = |vm: &Self, name: &[u8]| {
            matches!(vm.functions.get(name), Some(crate::func::Func::Builtin(_)))
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.ini.get_long("zend.assertions").hash(&mut h);
        self.optimization_level().hash(&mut h);
        self.ini.get_bool("opcache.save_comments").hash(&mut h);
        self.ini.get_bool("short_open_tag").hash(&mut h);
        builtin(self, b"sprintf").hash(&mut h);
        builtin(self, b"array_map").hash(&mut h);
        builtin(self, b"strlen").hash(&mut h);
        self.special_builtins().hash(&mut h);
        self.ct_constants().fingerprint().hash(&mut h);
        Some(h.finish())
    }

    /// The optimizer passes a file is compiled with: opcache's
    /// `opcache.optimization_level` when opcache optimizes (it is on for
    /// this SAPI, or only the file cache is), else none.
    fn optimization_level(&self) -> u32 {
        if self.ini.get_bool("opcache.enable")
            && (self.ini.get_bool("opcache.enable_cli")
                || self.ini.get_bool("opcache.file_cache_only"))
        {
            // A zend_long whose low 32 bits are the passes: `-1` is all.
            u32::try_from(self.ini.get_long("opcache.optimization_level") & 0xFFFF_FFFF)
                .unwrap_or(0)
        } else {
            0
        }
    }

    /// Makes a unit from the script cache current, repeating what
    /// compiling it did ([`compiler::UnitMeta`]): the anonymous class
    /// counter, `__COMPILER_HALT_OFFSET__`, the auto-globals it names and
    /// the registration of its declarations. Returns `false` (the caller
    /// then compiles the file) when the unit declares anonymous classes
    /// named for a different counter than the current one.
    pub fn install_cached_unit(&mut self, unit: &Rc<CompiledUnit>, filename: &ZStr) -> bool {
        let meta = &unit.meta;
        let declares_anon = unit.anon_end != meta.anon_base;
        if declares_anon && meta.anon_base != self.anon_counter {
            return false;
        }
        if declares_anon {
            self.anon_counter = unit.anon_end;
        }
        for &i in &meta.auto_globals {
            crate::auto_globals::note_compiled_index(i);
        }
        self.activate_compiled_auto_globals();
        if let Some(off) = meta.halt_offset {
            self.ext.halt_offsets.insert(filename.clone(), off);
        }
        if declares_anything(unit) {
            self.units.push(unit.clone());
        }
        true
    }

    /// The constants the compiler may substitute (`zend_try_ct_eval_const`,
    /// `can_ct_eval_const` in `Zend/zend_compile.c`): the internal (not
    /// user-defined) constants with a scalar value that are not deprecated.
    /// Built once and rebuilt only when internal or deprecated constants
    /// are added or removed.
    pub(crate) fn ct_constants(&mut self) -> CtConstants {
        let key = (
            self.constants
                .len()
                .saturating_sub(self.user_constants.len()),
            self.deprecated_constants.len(),
        );
        if let Some((k, c)) = &self.ct_constants {
            if *k == key {
                return c.clone();
            }
        }
        // Sized for every constant so the table never rehashes while it
        // fills (2 700 internal constants rehashed eleven times before), and
        // the few user-defined and deprecated names are removed afterwards
        // instead of being looked up once per constant.
        let mut table: crate::class::NameMap<Value> =
            crate::class::name_map_with_capacity(self.constants.len());
        table.extend(
            self.constants
                .iter()
                .filter(|(_, v)| {
                    matches!(
                        v,
                        Value::Null
                            | Value::Bool(_)
                            | Value::Int(_)
                            | Value::Float(_)
                            | Value::Str(_)
                    )
                })
                .map(|(name, v)| (name.clone(), v.clone())),
        );
        for name in &self.user_constants {
            table.remove(name);
        }
        for name in self.deprecated_constants.keys() {
            table.remove(name);
        }
        let c = CtConstants::new(table);
        self.ct_constants = Some((key, c.clone()));
        c
    }

    /// Parses source with current INI and mbstring request settings.
    ///
    /// For eval, Zend scans in the effective internal encoding and bypasses
    /// Unicode detection. Files use `zend.script_encoding` and BOM detection.
    pub fn parse_source(
        &self,
        src: &[u8],
        is_eval: bool,
        skip_shebang: bool,
    ) -> Result<php_parser::Program, php_parser::ParseError> {
        let opts = php_parser::ParseOptions {
            short_open_tag: self.ini.get_bool("short_open_tag"),
            start_in_scripting: is_eval,
            skip_shebang,
            multibyte: self.ini.get_bool("zend.multibyte")
                && self.script_encoding_provider.is_some(),
            ..Default::default()
        };
        let Some(provider) = self.script_encoding_provider.filter(|_| opts.multibyte) else {
            return php_parser::parse(src, &opts);
        };
        let current = provider(self);
        let script_encodings: Vec<_> = if is_eval {
            current.options.internal_encoding.into_iter().collect()
        } else {
            current.script_encodings
        };
        let encoding = php_parser::ScriptEncodingOptions {
            script_encodings: &script_encodings,
            detect_unicode: !is_eval && self.ini.get_bool("zend.detect_unicode"),
            precision: usize::try_from(self.ini.get_long("precision")).unwrap_or(14),
            ..current.options
        };
        php_parser::parse_with_encoding(src, &opts, &encoding)
    }

    /// Parses and compiles source code, calling the compile hook for files.
    pub(crate) fn compile_source(
        &mut self,
        src: Rc<[u8]>,
        filename: &ZStr,
        is_eval: bool,
        skip_shebang: bool,
    ) -> VmResult<CompileOutcome> {
        if !is_eval && !UNCACHEABLE_STREAM.with(|c| c.replace(false)) {
            if let Some(hook) = COMPILE_HOOK.with(|h| h.get()) {
                return hook(self, src, filename, skip_shebang);
            }
        }
        self.compile_source_default(src, filename, is_eval, skip_shebang)
    }

    /// Parses and compiles source code with the default compiler pipeline.
    ///
    /// The source comes as the `Rc` the compiled unit keeps, so the caller's
    /// buffer is released before parsing and a large include is not held
    /// twice while its AST is alive.
    pub fn compile_source_default(
        &mut self,
        src: Rc<[u8]>,
        filename: &ZStr,
        is_eval: bool,
        skip_shebang: bool,
    ) -> VmResult<CompileOutcome> {
        let parsed = self.parse_source(&src, is_eval, skip_shebang);
        let program = match parsed {
            Ok(p) => p,
            Err(e) => {
                for w in &e.warnings {
                    let level = match w.level {
                        php_parser::ast::DiagnosticLevel::CompileWarning => 128,
                        php_parser::ast::DiagnosticLevel::Deprecated => 8192,
                        php_parser::ast::DiagnosticLevel::Warning => 2,
                    };
                    self.error_at(level, &w.message, filename, w.line)?;
                }
                return match e.kind {
                    php_parser::ErrorKind::Parse => {
                        let o = self.create_exception("ParseError", &e.message, 0)?;
                        self.set_exc_prop(&o, "file", Value::Str(filename.clone()));
                        self.set_exc_prop(&o, "line", Value::Int(i64::from(e.line)));
                        Ok(CompileOutcome::ParseError(o))
                    }
                    // `zend_throw_exception(zend_ce_compile_error, ...)`
                    // during parsing (bug75218).
                    php_parser::ErrorKind::Compile if e.thrown => {
                        let o = self.create_exception("CompileError", &e.message, 0)?;
                        self.set_exc_prop(&o, "file", Value::Str(filename.clone()));
                        self.set_exc_prop(&o, "line", Value::Int(i64::from(e.line)));
                        Ok(CompileOutcome::ParseError(o))
                    }
                    php_parser::ErrorKind::Compile => {
                        self.report_compile_error(&e.message, filename, e.line, false);
                        Ok(CompileOutcome::Fatal)
                    }
                };
            }
        };
        let opts = CompileOptions {
            filename: filename.clone(),
            is_eval,
            eval_class: None,
            assertions: self.ini.get_long("zend.assertions"),
            eval_parent: None,
            closure_name: None,
            strict_types: false,
            sprintf_builtin: matches!(
                self.functions.get(b"sprintf".as_slice()),
                Some(crate::func::Func::Builtin(_))
            ),
            array_map_builtin: matches!(
                self.functions.get(b"array_map".as_slice()),
                Some(crate::func::Func::Builtin(_))
            ),
            strlen_builtin: matches!(
                self.functions.get(b"strlen".as_slice()),
                Some(crate::func::Func::Builtin(_))
            ),
            special_builtins: self.special_builtins(),
            anon_base: self.anon_counter,
            ct_constants: Some(self.ct_constants()),
            stack_limit: self.compile_stack_limit(),
            user_opcodes: self.user_opcodes(),
            optimization_level: self.optimization_level(),
            strip_doc_comments: !is_eval && STRIP_DOC_COMMENTS.with(std::cell::Cell::get),
        };
        let anon_base = self.anon_counter;
        let halt_compiler_offset = program.halt_compiler_offset;
        // `opcache.opt_debug_level` & `ZEND_DUMP_AFTER_OPTIMIZER`: opcache
        // dumps each script it compiles (`zend_optimize_script`); eval'd
        // code is not cached.
        let dump_src = (!is_eval
            && self.ini.get_bool("opcache.enable")
            && (self.ini.get_bool("opcache.enable_cli")
                || self.ini.get_bool("opcache.file_cache_only"))
            && self.ini.get_long("opcache.opt_debug_level")
                & (compiler::optimizer::dump::DUMP_AFTER_OPTIMIZER
                    | compiler::optimizer::dump::DUMP_BEFORE_DFA_PASS
                    | compiler::optimizer::dump::DUMP_AFTER_DFA_PASS)
                != 0)
            .then(|| src.clone());
        // A multibyte script's AST is kept for the semantic text of
        // diagnostics; any other is released statement by statement while
        // it compiles. The compiler sees the function table as it stands
        // (`CG(function_table)`), for the `fbc` of calls by name.
        let functions = &self.functions;
        let ignore_other_files = !is_eval && IGNORE_OTHER_FILES.with(std::cell::Cell::get);
        let table = |lc: &[u8]| {
            functions
                .get(lc)
                .filter(|f| match f {
                    // `ZEND_COMPILE_IGNORE_OTHER_FILES`: a user function is
                    // seen only if this file declared it.
                    crate::func::Func::User(u) if ignore_other_files => {
                        u.code.filename.as_bytes() == filename.as_bytes()
                    }
                    _ => true,
                })
                .map(compiler::FunctionInfo::of)
        };
        let (compiled, kept) = if program.multibyte {
            let unit = compiler::compile_with_functions(&program, src, opts, &table);
            (unit, Some(program))
        } else {
            let unit = compiler::compile_program_with_functions(program, src, opts, &table);
            (unit, None)
        };
        let auto_globals = self.activate_compiled_auto_globals();
        let unit = match compiled {
            Ok(mut u) => {
                self.anon_counter = u.anon_end;
                u.meta = compiler::UnitMeta {
                    anon_base,
                    halt_offset: halt_compiler_offset.map(|off| i64::try_from(off).unwrap_or(0)),
                    auto_globals,
                };
                u
            }
            Err(e) => {
                // `compile_file` records the diagnostics of a compilation;
                // a fatal error emits them first, with the user error
                // handler disabled (Zend/zend.c, "Emit any delayed error
                // before handling fatal error").
                if is_eval {
                    for (level, msg, line) in &e.warnings {
                        self.error_at(*level, msg, filename, *line)?;
                    }
                } else {
                    self.emit_warnings_without_handler(&e.warnings, filename);
                }
                self.report_compile_error(&e.message, filename, e.line, e.is_parse);
                return Ok(CompileOutcome::Fatal);
            }
        };
        if let Some(off) = halt_compiler_offset {
            self.ext
                .halt_offsets
                .insert(filename.clone(), i64::try_from(off).unwrap_or(0));
        }
        if let Some(program) = kept {
            self.parsed_sources
                .borrow_mut()
                .insert(filename.clone(), Rc::new(program));
        }
        let unit = Rc::new(unit);
        if declares_anything(&unit) {
            self.units.push(unit.clone());
        }
        if is_eval {
            for (level, msg, line) in &unit.warnings {
                self.error_at(*level, msg, filename, *line)?;
            }
        } else {
            self.emit_compile_warnings(&unit, filename)?;
        }
        // `opcache.opt_debug_level`: the optimized op arrays, after the
        // compile warnings (php-src dumps from the optimizer, which runs
        // when the script is cached).
        if let Some(src) = dump_src {
            let level =
                u32::try_from(self.ini.get_long("opcache.optimization_level") & 0xFFFF_FFFF)
                    .unwrap_or(0);
            let functions = &self.functions;
            let table = |lc: &[u8]| functions.get(lc).map(compiler::FunctionInfo::of);
            let internal = |lc: &[u8]| match functions.get(lc) {
                Some(crate::func::Func::Builtin(b)) => (b.flags, b.return_type.clone()),
                _ => (0, None),
            };
            let end = compiler::optimizer::dump::main_line_end(&src, halt_compiler_offset);
            let debug = self.ini.get_long("opcache.opt_debug_level");
            // The DFA pass dumps each op array while it optimizes them,
            // before `zend_optimize_script` dumps the result.
            let mut out = compiler::optimizer::dump::dump_unit_dfa(
                &unit, &table, &internal, level, end, debug,
            );
            if debug & compiler::optimizer::dump::DUMP_AFTER_OPTIMIZER != 0 {
                out.extend(compiler::optimizer::dump::dump_unit(
                    &unit,
                    &table,
                    &internal,
                    level,
                    end,
                    "after optimizer",
                ));
            }
            self.flush_stdout();
            let _ = std::io::Write::write_all(&mut std::io::stderr(), &out);
        }
        Ok(CompileOutcome::Ok(unit))
    }

    /// Emits the diagnostics recorded while compiling a file
    /// (`zend_emit_recorded_errors` after `compile_file`, or opcache's
    /// replay of a cached script's warnings).
    ///
    /// php-src binds a file's top-level functions while it compiles, so:
    /// - a function that is already declared (or declared twice in the
    ///   file) is a compile-time fatal error, before which the recorded
    ///   diagnostics are emitted with the user error handler disabled;
    /// - an exception thrown by the user error handler leaves the file's
    ///   functions and early-bound classes declared, though the file does
    ///   not run (GH-17422).
    pub fn emit_compile_warnings(&mut self, unit: &CompiledUnit, filename: &ZStr) -> VmResult<()> {
        if unit.warnings.is_empty() {
            return Ok(());
        }
        if self.hoisted_function_conflict(unit) {
            self.emit_warnings_without_handler(&unit.warnings, filename);
            return Ok(());
        }
        for (level, msg, line) in &unit.warnings {
            if let Err(t) = self.error_at(*level, msg, filename, *line) {
                if matches!(t, Throw::Exception(_)) {
                    self.declare_hoisted(unit)?;
                }
                return Err(t);
            }
        }
        Ok(())
    }

    /// Whether declaring `unit`'s hoisted functions would fail with
    /// "Cannot redeclare function".
    fn hoisted_function_conflict(&self, unit: &CompiledUnit) -> bool {
        let funcs = unit.decls.functions.borrow();
        let mut seen: Vec<ZStr> = Vec::new();
        for &i in &unit.hoisted_functions {
            let Some(f) = funcs.get(i as usize) else {
                continue;
            };
            let lc = f.name.to_ascii_lowercase();
            if self.functions.contains_key(&lc) || seen.contains(&lc) {
                return true;
            }
            seen.push(lc);
        }
        false
    }

    /// Emits diagnostics with the user error handler disabled, as php-src
    /// does for the recorded ones before a fatal error.
    #[cold]
    fn emit_warnings_without_handler(&mut self, warnings: &[(i64, String, u32)], filename: &ZStr) {
        if warnings.is_empty() {
            return;
        }
        let handlers = std::mem::take(&mut self.error_handlers);
        for (level, msg, line) in warnings {
            let _ = self.error_at(*level, msg, filename, *line);
        }
        self.error_handlers = handlers;
    }

    /// Declares the hoisted functions and classes of a unit.
    pub(crate) fn declare_hoisted(&mut self, unit: &CompiledUnit) -> VmResult<()> {
        for &i in &unit.hoisted_functions {
            let f = unit.decls.functions.borrow()[i as usize].clone();
            let lc = ZStr::from(f.name.to_ascii_lowercase());
            if let Some(existing) = self.functions.get(&lc) {
                // A preloaded script included again: its preloaded
                // functions are not declared again.
                if self.is_preloaded_function(&f) {
                    continue;
                }
                let msg = match existing {
                    crate::func::Func::User(u) => format!(
                        "Cannot redeclare function {}() (previously declared in {}:{})",
                        f.name, u.code.filename, u.line_start
                    ),
                    crate::func::Func::Builtin(_) => {
                        format!("Cannot redeclare function {}()", f.name)
                    }
                };
                self.report_compile_error(&msg, &f.code.filename, f.line_start, false);
                return Err(Throw::Fatal);
            }
            self.observer_declared(&lc, false);
            crate::builtins::info::note_user_function(&lc);
            self.functions.insert(lc, crate::func::Func::User(f));
        }
        let mut progress = true;
        while progress {
            progress = false;
            for &i in &unit.hoisted_classes {
                let d = unit.decls.classes.borrow()[i as usize].clone();
                if self
                    .classes
                    .contains_key(&ZStr::from(d.name.to_ascii_lowercase()))
                {
                    continue;
                }
                if self.can_early_bind(&d) {
                    // Early binding; errors surface when the declaration runs.
                    let saved = self.frames.len();
                    let r = self.declare_class(&d, false);
                    self.frames
                        .truncate(saved.max(self.frames.len().min(saved)));
                    if let Err(e) = r {
                        match e {
                            Throw::Fatal | Throw::Exit | Throw::FiberSuspend => return Err(e),
                            Throw::Exception(_) => {}
                        }
                    } else {
                        progress = true;
                    }
                }
            }
        }
        Ok(())
    }

    /// php-src's C stack at the start of a compilation, for the compiler's
    /// `zend_check_stack_limit()` model: the usable stack
    /// (`zend.max_allowed_stack_size - zend.reserved_stack_size`, whose 0
    /// default reserves `ZEND_ALLOCA_MAX_SIZE + 16 KiB`; with an automatic
    /// maximum, the one the runtime checks use) and the bytes in use (the
    /// native stack used since the first execution, scaled to php-src's
    /// frames as `native_stack_exhausted` does, plus php-src's frames from
    /// `main()` down to the first `zend_compile_expr()`).
    #[cold]
    fn compile_stack_limit(&self) -> Option<compiler::CompileStackLimit> {
        const DEFAULT_RESERVED: usize = 48 << 10;
        const COMPILE_BASE_BYTES: usize = 24 << 10;
        /// `zend_call_stack_init()` when the stack cannot be detected:
        /// `zend_call_stack_default_size()` (8 MiB on Linux) less the
        /// 32 KiB it takes off because the base is not the real one.
        const UNDETECTED_STACK_BYTES: usize = (8 << 20) - (32 << 10);
        let reserved = match usize::try_from(self.ini.get_long("zend.reserved_stack_size")) {
            Ok(0) | Err(_) => DEFAULT_RESERVED,
            Ok(r) => r,
        };
        let stack = match usize::try_from(self.ini.get_long("zend.max_allowed_stack_size")) {
            // -1: unchecked.
            Err(_) => return None,
            // 0: detect; a thread of unknown stack size (declared 0) gets
            // php-src's fallback instead of an empty budget.
            Ok(0) if self.native_stack_size == 0 => UNDETECTED_STACK_BYTES,
            Ok(0) => self.native_stack_size / crate::fiber::STACK_SCALE,
            Ok(m) => m,
        };
        let marker = 0u8;
        let here = std::ptr::addr_of!(marker) as usize;
        let running = self.native_stack_base.saturating_sub(here) / crate::fiber::STACK_SCALE;
        Some(compiler::CompileStackLimit {
            used: COMPILE_BASE_BYTES.saturating_add(running),
            size: stack.saturating_sub(reserved),
        })
    }

    /// Runs the main script; returns normally after shutdown work is done
    /// by the caller.
    pub fn run_main(&mut self, src: &[u8], path: &ZStr) -> Result<(), Throw> {
        self.run_top_level(src, path, false)
    }

    /// Models `zend_call_stack_overflowed()` in `zend_hash_compare` as a
    /// nesting limit for array comparisons: the usable stack
    /// (`zend.max_allowed_stack_size` minus `zend.reserved_stack_size`,
    /// whose 0 default is 48 KiB) over the C stack one nesting level of
    /// `zend_compare` takes.
    #[cold]
    fn apply_compare_depth_limit(&mut self) {
        /// Approximate C stack bytes per nesting level of `zend_compare` /
        /// `zend_hash_compare_impl` / `hash_zval_compare_function`.
        const COMPARE_FRAME_BYTES: usize = 256;
        let max = self.ini.get_long("zend.max_allowed_stack_size");
        let reserved = match usize::try_from(self.ini.get_long("zend.reserved_stack_size")) {
            Ok(0) | Err(_) => 48 << 10,
            Ok(r) => r,
        };
        let limit = match usize::try_from(max) {
            Err(_) => usize::MAX,
            Ok(0) => self.native_stack_size.saturating_sub(reserved) / COMPARE_FRAME_BYTES,
            Ok(m) => m.saturating_sub(reserved) / COMPARE_FRAME_BYTES,
        };
        php_runtime::ops::set_compare_depth_limit(limit.max(1));
    }

    /// Runs top-level eval input, as CLI `-r`, `-B`, `-R`, and `-E` do.
    /// Input has no opening tag and uses Zend's configured internal encoding.
    pub fn run_eval(&mut self, src: &[u8], path: &ZStr) -> Result<(), Throw> {
        self.run_top_level(src, path, true)
    }

    fn run_top_level(&mut self, src: &[u8], path: &ZStr, is_eval: bool) -> Result<(), Throw> {
        self.script_path = path.clone();
        self.apply_compare_depth_limit();
        if !is_eval {
            // php_execute_script adds the primary script to
            // EG(included_files): `require_once` of it is a no-op (bug60825).
            use std::os::unix::ffi::OsStrExt;
            if let Ok(real) = std::fs::canonicalize(std::ffi::OsStr::from_bytes(path.as_bytes())) {
                self.included_set
                    .insert(ZStr::from(real.as_os_str().as_bytes()));
            }
        }
        let src = if is_eval {
            src.to_vec()
        } else {
            match self.filter_source(path, src.to_vec()) {
                Ok(s) => s,
                Err(t) => return self.handle_top_throw(t),
            }
        };
        let unit = match self.compile_source(Rc::from(src), path, is_eval, !is_eval) {
            Ok(CompileOutcome::Ok(u)) => u,
            Ok(CompileOutcome::ParseError(e)) => {
                self.report_uncaught(&e);
                return Err(Throw::Fatal);
            }
            Ok(CompileOutcome::Fatal) => return Err(Throw::Fatal),
            Err(t) => return self.handle_top_throw(t),
        };
        let globals = self.globals.clone();
        let mut f = Frame::with_vars(unit.main.code.clone(), FrameKind::Main, globals, None);
        f.ufunc = None;
        self.frames.push(f);
        let base = self.frames.len() - 1;
        if let Err(t) = self.declare_hoisted(&unit) {
            self.frames.truncate(base);
            return self.handle_top_throw(t);
        }
        self.observer_begin_top();
        let r = self.execute(base);
        if let Some(f) = self.frames.get_mut(base) {
            f.attach_symbol_table();
        }
        self.frames.truncate(base);
        match r {
            Ok(v) => {
                self.main_return = v;
                Ok(())
            }
            Err(t) => self.handle_top_throw(t),
        }
    }

    /// Compiles and runs `src` as code an internal function executes
    /// directly (`zend_compile_file()` + `zend_execute()`, as
    /// `Phar::webPhar()` runs the requested entry): global scope, compiled
    /// under `path` like an included file, and a throw propagates to the
    /// calling builtin instead of being reported here. Backtraces skip the
    /// frame and show the builtin's own (`zend_fetch_debug_backtrace`).
    pub fn run_nested_script(&mut self, src: &[u8], path: &ZStr) -> Result<(), Throw> {
        let src = self.filter_source(path, src.to_vec())?;
        let unit = match self.compile_source(Rc::from(src), path, false, true)? {
            CompileOutcome::Ok(u) => u,
            CompileOutcome::ParseError(e) => return Err(Throw::Exception(e)),
            CompileOutcome::Fatal => return Err(Throw::Fatal),
        };
        self.declare_hoisted(&unit)?;
        let globals = self.globals.clone();
        let f = Frame::with_vars(unit.main.code.clone(), FrameKind::Include, globals, None);
        self.frames.push(f);
        self.observer_begin_top();
        let base = self.frames.len() - 1;
        let r = self.execute(base);
        if let Some(f) = self.frames.get_mut(base) {
            f.attach_symbol_table();
        }
        self.frames.truncate(base);
        r.map(|_| ())
    }

    /// Handles a throw escaping the top-level script.
    pub(crate) fn handle_top_throw(&mut self, t: Throw) -> Result<(), Throw> {
        match t {
            Throw::Exception(e) => self.uncaught_exception(e),
            other => Err(other),
        }
    }

    /// Whether the source filter may rewrite what is read from `path`; a
    /// script cache must then not serve it.
    fn source_filter_may_rewrite(&self, path: &ZStr) -> bool {
        self.source_filter.is_some() && self.source_filter_probe.is_none_or(|probe| probe(path))
    }

    fn resolve_include_path(&self, name: &[u8]) -> Option<std::path::PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let p = std::path::Path::new(std::ffi::OsStr::from_bytes(name));
        if name.is_empty() {
            return None;
        }
        let is_relative_explicit = name.starts_with(b"./") || name.starts_with(b"../");
        if p.is_absolute() || is_relative_explicit {
            return p.is_file().then(|| p.to_path_buf());
        }
        let ip = self
            .ini
            .get("include_path")
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| b".".to_vec());
        for dir in ip.split(|&c| c == b':') {
            if dir.is_empty() {
                continue;
            }
            let d = std::path::Path::new(std::ffi::OsStr::from_bytes(dir));
            let cand = d.join(p);
            if cand.is_file() {
                return Some(cand);
            }
        }
        // The directory of the executing script.
        let (file, _) = self.current_location();
        let fb = file.as_bytes();
        if let Some(pos) = fb.iter().rposition(|&c| c == b'/') {
            let d = std::path::Path::new(std::ffi::OsStr::from_bytes(&fb[..pos]));
            let cand = d.join(p);
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    }

    /// `include`/`require`/`eval`. Returns `true` if a frame was pushed.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn op_include(&mut self, kind: IncludeKind, v: Value, dst: u32) -> VmResult<bool> {
        let s = self.to_zstr(&v)?;
        if kind == IncludeKind::Eval {
            let (file, line) = self.current_location();
            let fname = ZStr::from(format!(
                "{}({}) : eval()'d code",
                file.to_string_lossy(),
                line
            ));
            let unit = match self.compile_source(Rc::from(s.as_bytes()), &fname, true, false)? {
                CompileOutcome::Ok(u) => u,
                CompileOutcome::ParseError(e) => return Err(Throw::Exception(e)),
                CompileOutcome::Fatal => return Err(Throw::Fatal),
            };
            self.declare_hoisted(&unit)?;
            self.push_include_frame(&unit, FrameKind::Eval, dst, None);
            return Ok(true);
        }
        let kind_name = match kind {
            IncludeKind::Include => "include",
            IncludeKind::IncludeOnce => "include_once",
            IncludeKind::Require => "require",
            IncludeKind::RequireOnce => "require_once",
            IncludeKind::Eval => "eval",
        };
        let name = s.as_bytes();
        if name.contains(&0) {
            return Err(self.make_throw(
                "ValueError",
                &format!("{kind_name}(): Argument #1 ($filename) must not contain any null bytes"),
            ));
        }
        // Extension opener (stream wrappers, include_path entries such as
        // `phar://`): `php_stream_open_for_zend_ex`.
        if let Some(opener) = self.include_opener {
            match opener(self, name, kind_name)? {
                crate::ext::IncludeOpen::Opened {
                    source,
                    opened_path,
                } => {
                    let uncacheable = is_uncacheable_stream_path(name);
                    return self.include_source(
                        kind,
                        kind_name,
                        source,
                        opened_path,
                        dst,
                        uncacheable,
                    );
                }
                crate::ext::IncludeOpen::Failed => {
                    return self.include_failed(kind, kind_name, &s, dst);
                }
                crate::ext::IncludeOpen::NotHandled => {}
            }
        }
        // A script cache is asked for an absolute name before the file is
        // looked up, as `persistent_compile_file` finds a script by
        // `accel_make_persistent_key` (the name itself) before opening it:
        // a hit costs no `stat`. The script counts as included under the
        // path it was compiled under. A name already included once is
        // answered first, so `include_once` does not load it again.
        // Only a normalized name is its own key; one with `.`/`..`
        // segments is resolved below through the realpath cache.
        let mut asked: Option<&[u8]> = None;
        if is_normalized_absolute(name) {
            if matches!(kind, IncludeKind::IncludeOnce | IncludeKind::RequireOnce)
                && self.included_set.contains(&s)
            {
                self.set_tmp(dst, Value::Bool(true));
                return Ok(false);
            }
            if !self.source_filter_may_rewrite(&s) {
                if let Some(hook) = PRECOMPILED_HOOK.with(std::cell::Cell::get) {
                    if let Some(unit) = hook(self, &s)? {
                        let path = unit.main.code.filename.clone();
                        return self.include_cached(kind, kind_name, &unit, path, dst);
                    }
                    asked = Some(name);
                }
            }
        }
        let resolved = self.resolve_include_path(name);
        let Some(path) = resolved else {
            if !name.is_empty() {
                if let Some(hook) = CACHED_INCLUDE_HOOK.with(std::cell::Cell::get) {
                    if let Some((path, unit)) = hook(self, name)? {
                        return self.include_cached(kind, kind_name, &unit, path, dst);
                    }
                }
            }
            if name.is_empty() {
                self.warning(&format!("{kind_name}(): Filename cannot be empty"))?;
            } else {
                self.include_warning(
                    kind_name,
                    name,
                    "Failed to open stream: No such file or directory",
                )?;
            }
            return self.include_failed(kind, kind_name, &s, dst);
        };
        use std::os::unix::ffi::OsStrExt;
        let real_z = ZStr::from(opened_path(&path).as_os_str().as_bytes());
        if matches!(kind, IncludeKind::IncludeOnce | IncludeKind::RequireOnce)
            && self.included_set.contains(&real_z)
        {
            self.set_tmp(dst, Value::Bool(true));
            return Ok(false);
        }
        // A script cache serves the include without the file being read,
        // unless the source filter may rewrite what would be read. It is
        // not asked twice for the same path.
        if asked != Some(real_z.as_bytes()) && !self.source_filter_may_rewrite(&real_z) {
            if let Some(hook) = PRECOMPILED_HOOK.with(std::cell::Cell::get) {
                if let Some(unit) = hook(self, &real_z)? {
                    if self.included_set.insert(real_z.clone()) {
                        self.included.push(real_z);
                    }
                    self.declare_hoisted(&unit)?;
                    self.push_include_frame(&unit, FrameKind::Include, dst, Some(kind_name));
                    return Ok(true);
                }
            }
        }
        let src = match std::fs::read(&path) {
            Ok(s) => s,
            Err(e) => {
                // strerror() text, without Rust's " (os error N)" suffix.
                let text = e.to_string();
                let text = text.split(" (os error").next().unwrap_or_default();
                self.include_warning(kind_name, name, &format!("Failed to open stream: {text}"))?;
                return self.include_failed(kind, kind_name, &s, dst);
            }
        };
        // The unit is compiled under the opened path, as `open_file_for_scanning`
        // uses `file_handle->opened_path` when there is one: `__FILE__`,
        // `__DIR__`, reflection's `getFileName()`, backtraces and messages
        // all show the resolved path, not the `..`/symlink spelling used.
        if self.included_set.insert(real_z.clone()) {
            self.included.push(real_z.clone());
        }
        let src = self.filter_source(&real_z, src)?;
        let unit = match self.compile_source(Rc::from(src), &real_z, false, true)? {
            CompileOutcome::Ok(u) => u,
            CompileOutcome::ParseError(e) => return Err(Throw::Exception(e)),
            CompileOutcome::Fatal => return Err(Throw::Fatal),
        };
        self.declare_hoisted(&unit)?;
        self.push_include_frame(&unit, FrameKind::Include, dst, Some(kind_name));
        Ok(true)
    }

    /// Runs a unit a script cache served for `path`.
    fn include_cached(
        &mut self,
        kind: IncludeKind,
        kind_name: &str,
        unit: &Rc<CompiledUnit>,
        path: ZStr,
        dst: u32,
    ) -> VmResult<bool> {
        if matches!(kind, IncludeKind::IncludeOnce | IncludeKind::RequireOnce)
            && self.included_set.contains(&path)
        {
            self.set_tmp(dst, Value::Bool(true));
            return Ok(false);
        }
        if self.included_set.insert(path.clone()) {
            self.included.push(path);
        }
        self.declare_hoisted(unit)?;
        self.push_include_frame(unit, FrameKind::Include, dst, Some(kind_name));
        Ok(true)
    }

    /// Includes already-opened source (from an opener hook).
    fn include_source(
        &mut self,
        kind: IncludeKind,
        kind_name: &str,
        src: Vec<u8>,
        path: ZStr,
        dst: u32,
        uncacheable: bool,
    ) -> VmResult<bool> {
        if matches!(kind, IncludeKind::IncludeOnce | IncludeKind::RequireOnce)
            && self.included_set.contains(&path)
        {
            self.set_tmp(dst, Value::Bool(true));
            return Ok(false);
        }
        if self.included_set.insert(path.clone()) {
            self.included.push(path.clone());
        }
        let src = self.filter_source(&path, src)?;
        // The flag is read once, when `compile_source` chooses the hook.
        let saved = UNCACHEABLE_STREAM.with(|c| c.replace(uncacheable));
        let compiled = self.compile_source(Rc::from(src), &path, false, true);
        UNCACHEABLE_STREAM.with(|c| c.set(saved));
        let unit = match compiled? {
            CompileOutcome::Ok(u) => u,
            CompileOutcome::ParseError(e) => return Err(Throw::Exception(e)),
            CompileOutcome::Fatal => return Err(Throw::Fatal),
        };
        self.declare_hoisted(&unit)?;
        self.push_include_frame(&unit, FrameKind::Include, dst, Some(kind_name));
        Ok(true)
    }

    /// The "Failed opening" tail of a failed include (the opener already
    /// reported why).
    fn include_failed(
        &mut self,
        kind: IncludeKind,
        kind_name: &str,
        s: &ZStr,
        dst: u32,
    ) -> VmResult<bool> {
        let n = s.to_string_lossy();
        let ip = self
            .ini
            .get("include_path")
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .unwrap_or_default();
        // `php_message_handler_for_zend`: ZMSG_FAILED_REQUIRE_FOPEN throws.
        if matches!(kind, IncludeKind::Require | IncludeKind::RequireOnce) {
            return Err(self.make_throw(
                "Error",
                &format!("Failed opening required '{n}' (include_path='{ip}')"),
            ));
        }
        self.include_warning(
            kind_name,
            s.as_bytes(),
            &format!("Failed opening '{n}' for inclusion (include_path='{ip}')"),
        )?;
        self.set_tmp(dst, Value::Bool(false));
        Ok(false)
    }

    /// Runs the source-filter hook (e.g. phar stubs) over script source.
    pub(crate) fn filter_source(&mut self, path: &ZStr, src: Vec<u8>) -> VmResult<Vec<u8>> {
        match self.source_filter {
            Some(f) => f(self, path, src),
            None => Ok(src),
        }
    }

    fn push_include_frame(
        &mut self,
        unit: &Rc<CompiledUnit>,
        kind: FrameKind,
        dst: u32,
        kind_name: Option<&str>,
    ) {
        self.frame_mut().attach_symbol_table();
        self.frame_mut().sync_cvs_to_symtab();
        let vars = self.frame().vars.clone();
        let mut f = Frame::with_vars(unit.main.code.clone(), kind, vars, None);
        let cur = self.frame();
        f.this = cur.this.clone();
        f.scope = cur.scope.clone();
        f.called = cur.called.clone();
        f.ret = RetTarget::Tmp(dst);
        f.builtin_name = kind_name.map(ZStr::from);
        self.frames.push(f);
        self.observer_begin_top();
    }

    /// Runs a file as `include` would from builtin code (e.g. the
    /// `auto_prepend_file`, `spl_autoload()`), compiled under its opened
    /// path like an included file.
    pub fn include_file(&mut self, path: &str) -> Result<(), Throw> {
        use std::os::unix::ffi::OsStrExt;
        let src = match std::fs::read(path) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        let p = ZStr::from(
            opened_path(std::path::Path::new(path))
                .as_os_str()
                .as_bytes(),
        );
        let unit = match self.compile_source(Rc::from(src), &p, false, true)? {
            CompileOutcome::Ok(u) => u,
            CompileOutcome::ParseError(e) => return self.handle_top_throw(Throw::Exception(e)),
            CompileOutcome::Fatal => return Err(Throw::Fatal),
        };
        self.declare_hoisted(&unit)?;
        let globals = self.globals.clone();
        let f = Frame::with_vars(unit.main.code.clone(), FrameKind::Main, globals, None);
        self.frames.push(f);
        self.observer_begin_top();
        let base = self.frames.len() - 1;
        let r = self.execute(base);
        if let Some(f) = self.frames.get_mut(base) {
            f.attach_symbol_table();
        }
        self.frames.truncate(base);
        match r {
            Ok(_) => Ok(()),
            Err(t) => self.handle_top_throw(t),
        }
    }
}

/// Whether `name` is an absolute path without empty, `.` or `..`
/// segments and without a trailing slash: such a name needs no resolving
/// to serve as a script cache key (symbolic links aside, which a key may
/// keep, as `accel_make_persistent_key` does).
fn is_normalized_absolute(name: &[u8]) -> bool {
    name.len() > 1
        && name.first() == Some(&b'/')
        && name.last() != Some(&b'/')
        && name[1..]
            .split(|&c| c == b'/')
            .all(|seg| !seg.is_empty() && seg != b"." && seg != b"..")
}

/// The `opened_path` of a plain file found at `path` for inclusion: its
/// real path, as `php_resolve_path()` (`tsrm_realpath`) resolves it before
/// `_php_stream_fopen` opens it with `STREAM_ASSUME_REALPATH`, so `..`
/// segments and symbolic links are resolved. If that fails the path is
/// only made absolute (`expand_filepath`).
///
/// With a realpath hook installed (the stream layer's `tsrm_realpath`),
/// resolution goes through `CWDG(realpath_cache)` as in php-src, so a
/// file included again (`include_once` of an autoloaded class, a file in a
/// directory already seen) costs a cache lookup instead of a `readlink`
/// for every path component.
fn opened_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    if let Some(hook) = REALPATH_HOOK.with(std::cell::Cell::get) {
        if let Some(real) = hook(path.as_os_str().as_bytes()) {
            return std::path::PathBuf::from(std::ffi::OsString::from_vec(real));
        }
        return std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    }
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Whether a compiled unit declares functions, closures, classes or
/// attributed constants: only such units stay registered in
/// [`Vm::units`] (Reflection looks closures up there and
/// preloading images them). A unit that declares nothing is released once
/// its code has run and nothing else holds it, like the op array php-src's
/// `zend_include_or_eval` / `zend_eval_stringl` destroys after executing an
/// included file or `eval()`'d string; keeping every such unit made each
/// `eval()` call retain its op array for the rest of the request.
fn declares_anything(unit: &CompiledUnit) -> bool {
    !unit.hoisted_functions.is_empty()
        || !unit.hoisted_classes.is_empty()
        || !unit.decls.functions.borrow().is_empty()
        || !unit.decls.classes.borrow().is_empty()
        || !unit.decls.const_attributes.borrow().is_empty()
}

impl CompiledUnit {
    /// Clears run-time caches on all op arrays in this unit so the unit
    /// carries no request state.
    pub fn clear_runtime_caches(&self) {
        self.main.code.clear_runtime_caches();
        self.main.statics.borrow_mut().clear();
        for f in self.decls.functions.borrow().iter() {
            f.code.clear_runtime_caches();
            f.statics.borrow_mut().clear();
        }
        for c in self.decls.classes.borrow().iter() {
            for m in &c.methods {
                m.func.code.clear_runtime_caches();
                m.func.statics.borrow_mut().clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_normalized_absolute, is_uncacheable_stream_path, opened_path};
    use crate::vm::Vm;
    use std::path::{Path, PathBuf};

    #[test]
    fn only_normalized_absolute_names_are_their_own_cache_key() {
        assert!(is_normalized_absolute(b"/app/vendor/autoload.php"));
        assert!(!is_normalized_absolute(b"/"));
        assert!(!is_normalized_absolute(b"app/a.php"));
        assert!(!is_normalized_absolute(b"/app/vendor/composer/../a.php"));
        assert!(!is_normalized_absolute(b"/app/./a.php"));
        assert!(!is_normalized_absolute(b"/app//a.php"));
        assert!(!is_normalized_absolute(b"/app/dir/"));
    }

    #[test]
    fn stream_paths_other_than_file_and_phar_are_not_cacheable() {
        assert!(is_uncacheable_stream_path(
            b"php://filter/read=string.toupper/resource=a.inc"
        ));
        assert!(is_uncacheable_stream_path(b"data://text/plain,x"));
        assert!(!is_uncacheable_stream_path(b"file:///tmp/a.inc"));
        assert!(!is_uncacheable_stream_path(b"phar:///tmp/a.phar/b.php"));
        assert!(!is_uncacheable_stream_path(b"/tmp/a.inc"));
        assert!(!is_uncacheable_stream_path(b"://x"));
        assert!(!is_uncacheable_stream_path(b"data:text/plain,x"));
    }

    fn fake_realpath(path: &[u8]) -> Option<Vec<u8>> {
        path.ends_with(b"found.php")
            .then(|| b"/real/found.php".to_vec())
    }

    // Each test runs on its own thread, so the thread-local hook set here
    // does not leak into other tests.
    #[test]
    fn opened_path_goes_through_the_realpath_hook() {
        assert_eq!(opened_path(Path::new("/")), PathBuf::from("/"));
        Vm::set_include_realpath_hook(fake_realpath);
        assert_eq!(
            opened_path(Path::new("./a/../found.php")),
            PathBuf::from("/real/found.php")
        );
        // An unresolvable path is only made absolute (`expand_filepath`).
        assert_eq!(
            opened_path(Path::new("/nowhere/missing.php")),
            PathBuf::from("/nowhere/missing.php")
        );
    }
}
