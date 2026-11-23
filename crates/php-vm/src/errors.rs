// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Error reporting (`zend_error`, `php_error_cb`): user error handlers,
//! display, logging, fatal errors and uncaught exceptions.

use crate::include::CompileOutcome;
use crate::object;
use crate::vm::{FrameKind, LastError, Throw, Vm, VmResult};
use php_runtime::{Obj, Value, ZStr};
use std::rc::Rc;

/// `E_*` constants.
#[allow(dead_code)]
pub mod level {
    /// `E_ERROR`
    pub const ERROR: i64 = 1;
    /// `E_WARNING`
    pub const WARNING: i64 = 2;
    /// `E_PARSE`
    pub const PARSE: i64 = 4;
    /// `E_NOTICE`
    pub const NOTICE: i64 = 8;
    /// `E_CORE_ERROR`
    pub const CORE_ERROR: i64 = 16;
    /// `E_CORE_WARNING`
    pub const CORE_WARNING: i64 = 32;
    /// `E_COMPILE_ERROR`
    pub const COMPILE_ERROR: i64 = 64;
    /// `E_COMPILE_WARNING`
    pub const COMPILE_WARNING: i64 = 128;
    /// `E_USER_ERROR`
    pub const USER_ERROR: i64 = 256;
    /// `E_USER_WARNING`
    pub const USER_WARNING: i64 = 512;
    /// `E_USER_NOTICE`
    pub const USER_NOTICE: i64 = 1024;
    /// `E_STRICT`
    pub const STRICT: i64 = 2048;
    /// `E_RECOVERABLE_ERROR`
    pub const RECOVERABLE_ERROR: i64 = 4096;
    /// `E_DEPRECATED`
    pub const DEPRECATED: i64 = 8192;
    /// `E_USER_DEPRECATED`
    pub const USER_DEPRECATED: i64 = 16384;
    /// `E_ALL`
    pub const ALL: i64 = 30719;
}

/// The label printed for an error level.
pub fn label(l: i64) -> &'static str {
    match l {
        1 | 16 | 64 | 256 => "Fatal error",
        4096 => "Recoverable fatal error",
        2 | 32 | 128 | 512 => "Warning",
        4 => "Parse error",
        8 | 1024 => "Notice",
        2048 => "Strict Standards",
        8192 | 16384 => "Deprecated",
        _ => "Unknown error",
    }
}

fn is_fatal(l: i64) -> bool {
    matches!(l, 1 | 4 | 16 | 64 | 256 | 4096)
}

impl Vm {
    /// The file and line of the currently executing user code.
    pub fn current_location(&self) -> (ZStr, u32) {
        for f in self.frames.iter().rev() {
            if f.kind == FrameKind::Builtin {
                continue;
            }
            return (f.code.filename.clone(), f.line());
        }
        // `php_error_cb` with nothing executing.
        (ZStr::from("Unknown"), 0)
    }

    /// Whether code is executing.
    pub fn is_executing(&self) -> bool {
        self.frames.iter().any(|f| f.kind != FrameKind::Builtin)
    }

    /// Reports an error at the current location.
    pub fn error(&mut self, level: i64, msg: &str) -> VmResult<()> {
        let (file, line) = self.current_location();
        self.error_at(level, msg, &file, line)
    }

    /// Reports a docref-style error (`func(): msg`) from a builtin.
    pub(crate) fn docref(&mut self, level: i64, msg: &str) -> VmResult<()> {
        self.docref_bytes(level, msg.as_bytes())
    }

    /// The function `php_error_docref()` names (`get_active_function_name()`):
    /// the innermost builtin frame, as `Class::method` for methods.
    pub(crate) fn docref_function_name(&self) -> Option<String> {
        self.frames
            .iter()
            .rev()
            .find(|f| f.kind == FrameKind::Builtin)
            .and_then(|f| {
                let n = f.builtin_name.clone()?;
                Some(match &f.scope {
                    Some(c) if f.this.is_some() || f.called.is_some() => {
                        format!("{}::{}", object::display_name(&c.name), n)
                    }
                    _ => n.to_string_lossy(),
                })
            })
    }

    /// The parenthesised part of a `php_error_docref()` origin: the
    /// running builtin's arguments rendered as in stack traces when
    /// `error_include_args` is on (`zend_trace_current_function_args_string`),
    /// else empty.
    pub(crate) fn docref_args(&mut self) -> String {
        if !self.ini.get_bool("error_include_args") {
            return String::new();
        }
        let Some(idx) = self
            .frames
            .iter()
            .rposition(|f| f.kind == FrameKind::Builtin)
        else {
            return String::new();
        };
        let args = self.trace_frame_args(idx);
        self.trace_args_list(&args)
    }

    /// Under `error_include_args`, a library diagnostic that already starts
    /// with the running builtin's `name(): ` origin gets the arguments put
    /// into it, as `php_verror` builds the origin itself
    /// (`zend_trace_current_function_args_string`).
    pub(crate) fn origin_with_args(&mut self, msg: &str) -> Option<String> {
        if !self.ini.get_bool("error_include_args") {
            return None;
        }
        let name = self.docref_function_name()?;
        let rest = msg.strip_prefix(name.as_str())?.strip_prefix("(): ")?;
        let args = self.docref_args();
        Some(format!("{name}({args}): {rest}"))
    }

    /// The origin `include`/`require` warnings are prefixed with:
    /// `include()`, or `include('name')` under `error_include_args`
    /// (`php_verror` while `ZEND_INCLUDE_OR_EVAL` executes).
    pub fn include_error_origin(&mut self, kind: &str, name: &[u8]) -> String {
        if self.ini.get_bool("error_include_args") {
            let arg = self.trace_arg(&Value::Str(ZStr::from(name)));
            format!("{kind}({arg})")
        } else {
            format!("{kind}()")
        }
    }

    /// Binary-safe [`Vm::docref`].
    pub(crate) fn docref_bytes(&mut self, level: i64, msg: &[u8]) -> VmResult<()> {
        // `php_verror`: nothing is reported while a delayed OOM is pending
        // (the builtin is bailing out; the fatal follows the call).
        if php_memlimit::is_pending() {
            return Ok(());
        }
        match self.docref_function_name() {
            Some(n) => {
                let args = self.docref_args();
                if self.ini.get_bool("html_errors") {
                    return self.docref_html(level, &format!("{n}({args})"), &n, msg);
                }
                let mut m = format!("{n}({args}): ").into_bytes();
                m.extend_from_slice(msg);
                self.error_bytes(level, &m)
            }
            None => self.error_bytes(level, msg),
        }
    }

    /// `php_verror` under `html_errors`: the origin and the message are
    /// HTML-escaped, and with a non-empty `docref_root` the manual link
    /// `[<a href='{root}{page}{ext}'>{page}{ext}</a>]` follows the origin.
    /// `function` names the page: `function.name` or `class.method` (see
    /// [`docref_page`]).
    #[cold]
    #[inline(never)]
    pub(crate) fn docref_html(
        &mut self,
        level: i64,
        origin: &str,
        function: &str,
        msg: &[u8],
    ) -> VmResult<()> {
        let mut m = self.error_html_escape(origin.as_bytes());
        let root = self
            .ini
            .get("docref_root")
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        if !root.is_empty() {
            let mut page = docref_page(function).into_bytes();
            if let Some(ext) = self.ini.get("docref_ext") {
                page.extend_from_slice(ext);
            }
            m.extend_from_slice(b" [<a href='");
            m.extend_from_slice(&root);
            m.extend_from_slice(&page);
            m.extend_from_slice(b"'>");
            m.extend_from_slice(&page);
            m.extend_from_slice(b"</a>]");
        }
        m.extend_from_slice(b": ");
        m.extend_from_slice(&self.error_html_escape(msg));
        self.error_msg_html_ready = true;
        self.error_bytes(level, &m)
    }

    /// A warning raised while `include`/`require` executes (`php_verror`
    /// with the opline on `ZEND_INCLUDE_OR_EVAL`): `kind()` or
    /// `kind('name')` prefixes it, and under `html_errors` it is escaped and
    /// linked to the `function.kind` manual page.
    pub(crate) fn include_warning(&mut self, kind: &str, name: &[u8], msg: &str) -> VmResult<()> {
        self.include_error(level::WARNING, kind, name, msg)
    }

    /// [`Vm::include_warning`] at any level: the stream layer's warnings
    /// and notices while a wrapper `include` opens
    /// (`php_stream_open_for_zend_ex`, reported by the `php-ext-streams`
    /// include opener).
    pub fn include_error(
        &mut self,
        level: i64,
        kind: &str,
        name: &[u8],
        msg: &str,
    ) -> VmResult<()> {
        let origin = self.include_error_origin(kind, name);
        if self.ini.get_bool("html_errors") {
            return self.docref_html(level, &origin, kind, msg.as_bytes());
        }
        self.error(level, &format!("{origin}: {msg}"))
    }

    /// `escape_html` of `main/main.c` with the request's charset.
    fn error_html_escape(&self, msg: &[u8]) -> Vec<u8> {
        let internal_encoding = self.ini.get("internal_encoding");
        let default_charset = self.ini.get("default_charset");
        let charset =
            php_stdstr::html::effective_charset_hint(None, internal_encoding, default_charset);
        html_escape(msg, charset)
    }

    /// Reports an error at a given location.
    pub fn error_at(&mut self, level: i64, msg: &str, file: &ZStr, line: u32) -> VmResult<()> {
        self.error_at_bytes(level, msg.as_bytes(), file, line)
    }

    /// Reports an error with a binary message at the current location.
    pub fn error_bytes(&mut self, level: i64, msg: &[u8]) -> VmResult<()> {
        let (file, line) = self.current_location();
        self.error_at_bytes(level, msg, &file, line)
    }

    /// Reports an error (binary-safe message) at a given location.
    #[allow(clippy::too_many_lines)]
    pub fn error_at_bytes(
        &mut self,
        level: i64,
        msg: &[u8],
        file: &ZStr,
        line: u32,
    ) -> VmResult<()> {
        // A message `php_verror` composed as HTML applies to this report
        // only, not to errors raised by the user handler below.
        let html_ready = std::mem::take(&mut self.error_msg_html_ready);
        if is_fatal(level) {
            // "Emit any delayed error before handling fatal error": before
            // the pending exception, the backtrace and the user handler.
            crate::classlink::emit_recorded_before_fatal(self);
        }
        self.note_error_backtrace(level);
        // User error handler.
        let handleable = level
            & (level::ERROR
                | level::PARSE
                | level::CORE_ERROR
                | level::CORE_WARNING
                | level::COMPILE_ERROR
                | level::COMPILE_WARNING)
            == 0;
        if handleable {
            if let Some(h) = self.error_handlers.last().cloned() {
                if !h.callback.is_null() && h.mask & level != 0 {
                    self.in_error_handler += 1;
                    let args = vec![
                        Value::Int(level),
                        Value::Str(ZStr::from(msg)),
                        Value::Str(file.clone()),
                        Value::Int(i64::from(line)),
                    ];
                    // zend_error_impl: EG(user_error_handler) is UNDEF while
                    // the handler runs (so its own errors, and the callable
                    // deprecations of resolving it, take the default path or
                    // a handler it installs), and is put back afterwards
                    // unless the handler installed another one.
                    if let Some(top) = self.error_handlers.last_mut() {
                        top.callback = Value::Null;
                    }
                    let r = self.call_error_handler(&h.callback, args);
                    match self.error_handlers.last_mut() {
                        Some(top) if top.callback.is_null() => top.callback = h.callback,
                        Some(_) => {}
                        None => self.error_handlers.push(h),
                    }
                    self.in_error_handler -= 1;
                    let r = r?;
                    if !matches!(r, Value::Bool(false)) {
                        if level == level::USER_ERROR || level == level::RECOVERABLE_ERROR {
                            return Ok(());
                        }
                        return Ok(());
                    }
                }
            }
        }
        // `php_error_cb`: a repeated error is neither stored nor shown.
        if !self.is_repeated_error(msg, file, line) {
            self.last_error = Some(LastError {
                level,
                message: msg.to_vec(),
                file: file.clone(),
                line,
            });
            if self.error_reporting & level != 0
                || level & (level::CORE_ERROR | level::CORE_WARNING) != 0
            {
                self.error_msg_html_ready = html_ready;
                self.display_error(level, msg, file, line);
                self.error_msg_html_ready = false;
            }
        }
        if is_fatal(level) {
            self.exit_status = fatal_ends_request();
            object::set_destructors_enabled(false);
            return Err(Throw::Fatal);
        }
        Ok(())
    }

    /// `zend_error_impl`: every reported error clears
    /// `EG(last_fatal_error_backtrace)`. For a fatal one (`E_FATAL_ERRORS`)
    /// an exception still pending is reported first, as an "Uncaught ..."
    /// warning (`zend_exception_error(EG(exception), E_WARNING)`), and the
    /// fatal error then carries no backtrace; otherwise the stack is
    /// recorded for `php_error_cb` and `error_get_last()` when
    /// `fatal_error_backtraces` is on.
    fn note_error_backtrace(&mut self, level: i64) {
        if self.last_fatal_error_backtrace.is_some() {
            self.last_fatal_error_backtrace = None;
        }
        if !is_fatal(level) {
            return;
        }
        match self.take_pending_exception() {
            // `EG(exception) = NULL` after it: an exception its
            // `__toString()` threw goes too.
            Some(e) => {
                self.exception_error(&e, level::WARNING);
            }
            None => self.record_fatal_backtrace(),
        }
    }

    /// `EG(exception)` when a fatal error is raised: an exception a native
    /// caller still holds while it goes on (an output or header callback
    /// threw). Left alone while the delayed memory-limit error is reported
    /// (`EG(delayed_oom).state == ZEND_OOM_STATE_HANDLING`).
    fn take_pending_exception(&mut self) -> Option<Obj> {
        if self.reporting_oom || !matches!(self.pending_throw, Some(Throw::Exception(_))) {
            return None;
        }
        match self.pending_throw.take() {
            Some(Throw::Exception(e)) => Some(e),
            _ => None,
        }
    }

    /// `zend_fetch_debug_backtrace(&EG(last_fatal_error_backtrace), 0,
    /// EG(exception_ignore_args) ? DEBUG_BACKTRACE_IGNORE_ARGS : 0, 0)`.
    /// With nothing executing (a compile error of the main script) the
    /// trace is empty, which `zend_trace_to_string(..., include_main)`
    /// prints as `#0 {main}`.
    #[cold]
    #[inline(never)]
    fn record_fatal_backtrace(&mut self) {
        if self.ini.get_bool("fatal_error_backtraces") {
            let ignore_args = self.ini.get_bool("zend.exception_ignore_args");
            self.last_fatal_error_backtrace = Some(self.backtrace(false, ignore_args, false, 0));
        }
    }

    /// The `"\nStack trace:\n#0 ...\n#N {main}"` suffix `php_error_cb`
    /// appends to an error while a fatal backtrace is recorded
    /// (`zend_trace_to_string(..., include_main = true)`), else empty.
    fn fatal_backtrace_suffix(&mut self) -> String {
        match self.last_fatal_error_backtrace.clone() {
            Some(trace) => {
                let s = self.trace_to_string(&trace, true);
                format!("\nStack trace:\n{s}")
            }
            None => String::new(),
        }
    }

    /// `php_error_cb`'s repeated-error check: under
    /// `ignore_repeated_errors`, an error whose message equals the last
    /// stored one (and, unless `ignore_repeated_source` is on, whose file
    /// and line match too) is neither stored nor displayed or logged.
    fn is_repeated_error(&self, msg: &[u8], file: &ZStr, line: u32) -> bool {
        let Some(last) = &self.last_error else {
            return false;
        };
        if last.message != msg || !self.ini.get_bool("ignore_repeated_errors") {
            return false;
        }
        self.ini.get_bool("ignore_repeated_source") || (last.line == line && last.file == *file)
    }

    /// Displays/logs an error according to the ini settings.
    pub(crate) fn display_error(&mut self, level: i64, msg: &[u8], file: &ZStr, line: u32) {
        let lbl = label(level);
        let file_s = file.to_string_lossy();
        let trace = self.fatal_backtrace_suffix();
        if self.ini.get_bool("log_errors") {
            // php_error_cb: "PHP %s:  %s in %s on line %u%s%s".
            let mut log = format!("PHP {lbl}:  ").into_bytes();
            log.extend_from_slice(&php_log::escape_nul_for_log(msg));
            log.extend_from_slice(b" in ");
            log.extend_from_slice(file.as_bytes());
            log.extend_from_slice(format!(" on line {line}{trace}").as_bytes());
            self.log_err_with_severity(&log, php_log::severity_of_error_type(level));
        }
        let de = self
            .ini
            .get("display_errors")
            .map(<[u8]>::to_ascii_lowercase)
            .unwrap_or_default();
        let display = match de.as_slice() {
            b"stderr" => Some(true),
            b"stdout" | b"1" | b"on" | b"yes" | b"true" => Some(false),
            b"" | b"0" | b"off" | b"no" | b"false" | b"none" => None,
            _ => {
                if std::str::from_utf8(&de)
                    .ok()
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(0)
                    != 0
                {
                    Some(false)
                } else {
                    None
                }
            }
        };
        let Some(to_stderr) = display else {
            return;
        };
        let prepend = self
            .ini
            .get("error_prepend_string")
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        let append = self
            .ini
            .get("error_append_string")
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        if self.ini.get_bool("html_errors") {
            let mut out = prepend.clone();
            out.extend_from_slice(format!("<br />\n<b>{lbl}</b>:  ").as_bytes());
            // `php_error_cb`: the message is HTML-escaped with html_errors,
            // unless `php_verror` already escaped it (docref messages).
            if std::mem::take(&mut self.error_msg_html_ready) {
                out.extend_from_slice(msg);
            } else {
                out.extend_from_slice(&self.error_html_escape(msg));
            }
            // The `E_ERROR`/`E_PARSE` format has no stack trace.
            let trace = if matches!(level, level::ERROR | level::PARSE) {
                ""
            } else {
                trace.as_str()
            };
            out.extend_from_slice(
                format!(" in <b>{file_s}</b> on line <b>{line}</b><br />{trace}\n").as_bytes(),
            );
            out.extend_from_slice(&append);
            self.echo(&out);
            return;
        }
        if to_stderr {
            self.flush_stdout();
            let mut s = format!("{lbl}: ").into_bytes();
            s.extend_from_slice(msg);
            s.extend_from_slice(format!(" in {file_s} on line {line}{trace}\n").as_bytes());
            let _ = std::io::Write::write_all(&mut std::io::stderr(), &s);
            return;
        }
        let mut out = prepend;
        out.extend_from_slice(format!("\n{lbl}: ").as_bytes());
        out.extend_from_slice(msg);
        out.extend_from_slice(format!(" in {file_s} on line {line}{trace}\n").as_bytes());
        out.extend_from_slice(&append);
        self.echo(&out);
    }

    /// Reports a fatal error at the current location and returns the bailout.
    pub fn fatal(&mut self, msg: &str) -> Throw {
        let (file, line) = self.current_location();
        self.report_fatal(msg, &file, line);
        Throw::Fatal
    }

    /// A library reports that a block of `bytes` cannot be allocated: the
    /// `memory_limit` fatal when the size crosses the limit (recorded, the
    /// VM reports it where the error leaves the builtin), else the system
    /// refused it and the message is `zend_out_of_memory`'s.
    #[cold]
    pub fn out_of_memory(&mut self, bytes: usize) -> Throw {
        match php_memlimit::try_reserve(bytes) {
            Err(over) => Throw::from(over),
            Ok(()) => self.fatal(&format!("Out of memory (tried to allocate {bytes} bytes)")),
        }
    }

    /// Reports a fatal error (`E_ERROR`) without user handlers.
    pub(crate) fn report_fatal(&mut self, msg: &str, file: &ZStr, line: u32) {
        self.report_fatal_bytes(msg.as_bytes(), file, line);
    }

    fn report_fatal_bytes(&mut self, msg: &[u8], file: &ZStr, line: u32) {
        crate::classlink::emit_recorded_before_fatal(self);
        self.note_error_backtrace(level::ERROR);
        self.error_cb_dont_bail(level::ERROR, msg, file, line);
        object::set_destructors_enabled(false);
    }

    /// `php_error_cb` with `E_DONT_BAIL`, the path `zend_exception_error`
    /// takes: records and displays the error without the bailout's
    /// `zend_objects_store_mark_destructed`, so objects still get their
    /// destructors at shutdown after an uncaught exception. An `E_ERROR`
    /// sets the exit status.
    fn error_cb_dont_bail(&mut self, level: i64, msg: &[u8], file: &ZStr, line: u32) {
        self.last_error = Some(LastError {
            level,
            message: msg.to_vec(),
            file: file.clone(),
            line,
        });
        if self.error_reporting & level != 0 {
            self.display_error(level, msg, file, line);
        }
        if level == level::ERROR {
            self.exit_status = fatal_ends_request();
        }
    }

    /// Reports a compile error: an `E_COMPILE_ERROR` raised with
    /// `zend_error_noreturn()`, which carries the fatal backtrace, or
    /// (`parse`) the `E_PARSE` a `ParseError` stands for, which reaches
    /// `php_error_cb` only through `zend_exception_error` and so never
    /// carries one.
    pub(crate) fn report_compile_error(&mut self, msg: &str, file: &ZStr, line: u32, parse: bool) {
        crate::classlink::emit_recorded_before_fatal(self);
        if parse {
            self.compile_exception_error(level::PARSE, msg, file, line);
        } else {
            self.note_error_backtrace(level::COMPILE_ERROR);
            self.show_compile_error(level::COMPILE_ERROR, msg, file, line);
        }
    }

    /// Records and displays a compile error and bails out (destructors
    /// are disabled), without touching the fatal backtrace.
    fn show_compile_error(&mut self, lv: i64, msg: &str, file: &ZStr, line: u32) {
        self.last_error = Some(LastError {
            level: lv,
            message: msg.as_bytes().to_vec(),
            file: file.clone(),
            line,
        });
        if self.error_reporting & lv != 0 {
            self.display_error(lv, msg.as_bytes(), file, line);
        }
        self.exit_status = fatal_ends_request();
        object::set_destructors_enabled(false);
    }

    /// `zend_exception_error` of a `ParseError` or `CompileError`: the
    /// compile error it stands for, reported with `E_DONT_BAIL` (no fatal
    /// backtrace; the destructors still run at shutdown).
    fn compile_exception_error(&mut self, lv: i64, msg: &str, file: &ZStr, line: u32) {
        self.last_fatal_error_backtrace = None;
        let enabled = object::destructors_enabled();
        self.show_compile_error(lv, msg, file, line);
        object::set_destructors_enabled(enabled);
    }

    /// `php_lint_script()` (`php -l`): compiles `src` as the script
    /// `filename` without running it, and returns whether it compiled.
    /// Diagnostics are reported as a request reports them: compile
    /// warnings as they are, a `ParseError` through
    /// `zend_exception_error`, a compile error as the fatal error it is,
    /// with its backtrace.
    pub fn lint_script(&mut self, src: &[u8], filename: &ZStr) -> bool {
        let src = match self.filter_source(filename, src.to_vec()) {
            Ok(src) => src,
            Err(t) => return self.lint_failed(t),
        };
        match self.compile_source(Rc::from(src), filename, false, true) {
            Ok(CompileOutcome::Ok(_)) => true,
            Ok(CompileOutcome::ParseError(e)) => {
                self.report_uncaught(&e);
                false
            }
            Ok(CompileOutcome::Fatal) => false,
            Err(t) => self.lint_failed(t),
        }
    }

    /// A throw escaping the lint compile (an error handler of a compile
    /// warning threw): an uncaught exception is reported as such.
    fn lint_failed(&mut self, t: Throw) -> bool {
        if let Throw::Exception(e) = t {
            let _ = self.uncaught_exception(e);
        }
        false
    }

    /// `call_user_function()`: `zend_call_function()` without a
    /// `zend_fcall_info_cache`, for the callables the engine keeps as given
    /// (user error and exception handlers). `zend_is_callable_ex` resolves
    /// `cb` at every call, in the calling user frame's scope (the code that
    /// raised the error; none for an exception handler), raising the
    /// callable deprecations each time; the call is `ZEND_CALL_DYNAMIC`.
    /// A callable that does not resolve there is `invalid_callback`.
    pub(crate) fn call_user_function(&mut self, cb: &Value, args: Vec<Value>) -> VmResult<Value> {
        let mut call = match self.zend_is_callable(cb, false)? {
            Ok(call) => call,
            Err(reason) => return Err(self.invalid_callback(cb, &reason)),
        };
        call.dynamic = true;
        call.args = args.into_iter().map(Some).collect();
        self.call_pending(call)
    }

    /// `zend_error` calling the user error handler: the call is made from
    /// `EG(current_execute_data)`, so when a user frame is running (not a
    /// builtin, whose own frame is the caller) the handler counts as called
    /// from user code: its argument errors say "called in ... on line N".
    fn call_error_handler(&mut self, cb: &Value, args: Vec<Value>) -> VmResult<Value> {
        let from_internal = !self
            .frames
            .last()
            .is_some_and(|f| f.kind != FrameKind::Builtin);
        let mut call = match self.zend_is_callable(cb, false)? {
            Ok(call) => call,
            Err(reason) => return Err(self.invalid_callback(cb, &reason)),
        };
        call.dynamic = true;
        call.args = args.into_iter().map(Some).collect();
        self.call_pending_ex(call, from_internal)
    }

    /// `zend_throw_error(NULL, "Invalid callback %s, %s", ...)` from
    /// `zend_call_function()` (the callable's name as
    /// `zend_get_callable_name_ex` spells it): an `Error`; with no user
    /// frame executing (an exception handler run after the script ended,
    /// at shutdown) `zend_throw_error()` raises the fatal error instead,
    /// "in Unknown on line 0", and the shutdown functions still run.
    #[cold]
    #[inline(never)]
    fn invalid_callback(&mut self, cb: &Value, reason: &str) -> Throw {
        let msg = format!("Invalid callback {}, {reason}", self.callable_name(cb));
        if self.is_executing() {
            return self.make_throw("Error", &msg);
        }
        match self.error(level::ERROR, &msg) {
            Err(t) => t,
            Ok(()) => Throw::Fatal,
        }
    }

    /// Handles an exception that escaped all frames: the user exception
    /// handler or the "Uncaught" fatal error.
    ///
    /// `zend_user_exception_handler`: the handler is pushed and none is
    /// current while it runs; afterwards it becomes current again unless
    /// it installed another one (or restored an older one). The handler is
    /// resolved when it is called ([`Vm::call_user_function`]), with no
    /// frame: `'parent::h'` is an "Invalid callback" fatal error then. An
    /// exception the handler throws goes to the handler current at that
    /// point (`zend_throw_exception_internal` without a frame), else is
    /// fatal.
    pub(crate) fn uncaught_exception(&mut self, e: Obj) -> Result<(), Throw> {
        if let Some(h) = self.exception_handlers.last().cloned() {
            if !h.is_null() {
                self.exception_handlers.push(Value::Null);
                let r = match self.call_user_function(&h, vec![Value::Object(e)]) {
                    Ok(_) => Ok(()),
                    Err(Throw::Exception(e2)) => self.uncaught_exception(e2),
                    Err(t) => Err(t),
                };
                if self.exception_handlers.last().is_some_and(Value::is_null) {
                    self.exception_handlers.pop();
                }
                return r;
            }
        }
        self.report_uncaught(&e);
        Err(Throw::Fatal)
    }

    /// Prints "Fatal error: Uncaught ...".
    pub(crate) fn report_uncaught(&mut self, e: &Obj) {
        self.exception_error(e, level::ERROR);
    }

    /// `zend_exception_error(ex, severity)`: reports `e` as "Uncaught ...
    /// thrown" at `severity` (`E_ERROR` for an uncaught exception,
    /// `E_WARNING` for one still pending when a fatal error is raised, or
    /// for `assert()`'s with `assert.bail`), without a fatal backtrace (its
    /// own trace is in the message). A `ParseError` or `CompileError` is
    /// reported as the compile error it stands for; a fiber's graceful exit
    /// reports nothing. The reports go straight to the error callback
    /// (`zend_error_va` calls `zend_error_cb`), never to a user error
    /// handler. Returns the exception `e`'s `__toString()` threw, which
    /// php-src leaves in `EG(exception)`.
    pub(crate) fn exception_error(&mut self, e: &Obj, severity: i64) -> Option<Obj> {
        self.last_fatal_error_backtrace = None;
        let cls = object::class_of(e).clone();
        if cls.lc_name.as_bytes() == b"parseerror" || cls.lc_name.as_bytes() == b"compileerror" {
            let msg = self.exc_prop(e, "message");
            let file = self.exc_prop(e, "file");
            let line = self.exc_prop(e, "line");
            let msg = msg.as_str().map(ZStr::to_string_lossy).unwrap_or_default();
            let file = file.as_str().cloned().unwrap_or_default();
            let line = u32::try_from(line.as_int().unwrap_or(0)).unwrap_or(0);
            let lv = if cls.lc_name.as_bytes() == b"parseerror" {
                level::PARSE
            } else {
                level::COMPILE_ERROR
            };
            self.compile_exception_error(lv, &msg, &file, line);
            return None;
        }
        if crate::fiber::is_graceful_exit(e) {
            return None;
        }
        let mut thrown = None;
        match self.call_method_name(e, "__toString", Vec::new()) {
            Ok(v) => match v.deref() {
                Value::Str(s) => self.set_exc_prop(e, "string", Value::Str(s)),
                _ => {
                    let _ =
                        self.warning(&format!("{}::__toString() must return a string", cls.name));
                }
            },
            Err(Throw::Exception(inner)) => {
                // Thrown with no executing frame: php-src reports the inner
                // exception right away (`zend_throw_exception_internal`).
                if !self.is_executing() {
                    self.exception_error(&inner, level::ERROR);
                    return None;
                }
                let file = self
                    .exc_prop(&inner, "file")
                    .as_str()
                    .cloned()
                    .unwrap_or_default();
                let file = if file.is_empty() {
                    ZStr::from("Unknown")
                } else {
                    file
                };
                let line =
                    u32::try_from(self.exc_prop(&inner, "line").as_int().unwrap_or(0)).unwrap_or(0);
                let msg = format!(
                    "Uncaught {} in exception handling during call to {}::__toString()",
                    object::class_of(&inner).name,
                    cls.name
                );
                self.error_cb_dont_bail(level::WARNING, msg.as_bytes(), &file, line);
                thrown = Some(inner);
            }
            Err(_) => {}
        }
        let s = self.exc_prop(e, "string");
        let s = s.as_str().cloned().unwrap_or_default();
        let file = self
            .exc_prop(e, "file")
            .as_str()
            .cloned()
            .unwrap_or_default();
        let file = if file.is_empty() {
            ZStr::from("Unknown")
        } else {
            file
        };
        let line = u32::try_from(self.exc_prop(e, "line").as_int().unwrap_or(0)).unwrap_or(0);
        let mut msg = b"Uncaught ".to_vec();
        msg.extend_from_slice(s.as_bytes());
        msg.extend_from_slice(b"\n  thrown");
        self.error_cb_dont_bail(severity, &msg, &file, line);
        thrown
    }
}

/// The manual page `php_verror` links a docref error to: `function.name`,
/// or `class.method` for `Class::method`, with leading underscores of the
/// function stripped, `_` turned into `-` and lowercased.
fn docref_page(function: &str) -> String {
    let page = match function.split_once("::") {
        Some((class, method)) => format!("{class}.{}", method.trim_start_matches('_')),
        None => format!("function.{}", function.trim_start_matches('_')),
    };
    page.replace('_', "-").to_ascii_lowercase()
}

/// `escape_html` of `php_error_cb`: retry invalid input with substitution.
fn html_escape(msg: &[u8], charset: Option<&[u8]>) -> Vec<u8> {
    use php_stdstr::html::{escape_html_entities, ENT_COMPAT, ENT_SUBSTITUTE};

    let mut notices = php_stdstr::Notices::new();
    let out = escape_html_entities(
        msg,
        false,
        ENT_COMPAT,
        charset,
        true,
        true,
        "htmlspecialchars",
        &mut notices,
    );
    if out.is_empty() {
        return escape_html_entities(
            msg,
            false,
            ENT_COMPAT | ENT_SUBSTITUTE,
            charset,
            true,
            true,
            "htmlspecialchars",
            &mut notices,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{docref_page, level};
    use crate::vm::Throw;
    use crate::{Vm, VmOptions};
    use php_runtime::{Value, ZStr};
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn vm_with_sink() -> (Vm, Sink) {
        let sink = Sink::default();
        let vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
        (vm, sink)
    }

    fn output(vm: Vm, sink: &Sink) -> String {
        drop(vm);
        String::from_utf8_lossy(&sink.0.borrow()).into_owned()
    }

    #[test]
    fn docref_pages_follow_php_verror() {
        assert_eq!(docref_page("require_once"), "function.require-once");
        assert_eq!(docref_page("_strlen"), "function.strlen");
        assert_eq!(
            docref_page("SplFileObject::__construct"),
            "splfileobject.construct"
        );
        assert_eq!(docref_page("Foo_Bar::do_it"), "foo-bar.do-it");
    }

    /// `zend_error_impl` with `EG(exception)` set: the pending exception is
    /// reported as a warning (`zend_exception_error(..., E_WARNING)`) and
    /// the fatal error that follows carries no backtrace
    /// (Zend/tests/assert/gh16293_001: the warning, then no trace).
    #[test]
    fn fatal_error_with_a_pending_exception_reports_it_as_a_warning_first() {
        let (mut vm, sink) = vm_with_sink();
        let e = vm.create_exception("Exception", "Boo", 0).unwrap();
        vm.set_exc_prop(&e, "file", Value::Str(ZStr::from("t.php")));
        vm.set_exc_prop(&e, "line", Value::Int(3));
        vm.pending_throw = Some(Throw::Exception(e));
        let r = vm.error_at(level::ERROR, "boom", &ZStr::from("t.php"), 7);
        assert!(matches!(r, Err(Throw::Fatal)));
        assert!(vm.pending_throw.is_none());
        assert_eq!(vm.exit_status, 255);
        assert_eq!(
            output(vm, &sink),
            "\nWarning: Uncaught Exception: Boo in t.php:3\nStack trace:\n#0 {main}\n  thrown in t.php on line 3\n\nFatal error: boom in t.php on line 7\n"
        );
    }

    /// With nothing executing, `zend_fetch_debug_backtrace` yields an
    /// empty trace, which `zend_trace_to_string(..., include_main)` prints
    /// as `#0 {main}`: the compile errors of a main script get it too.
    #[test]
    fn frameless_fatal_error_has_a_main_only_backtrace() {
        let (mut vm, sink) = vm_with_sink();
        let r = vm.error_at(level::ERROR, "boom", &ZStr::from("t.php"), 7);
        assert!(matches!(r, Err(Throw::Fatal)));
        assert_eq!(
            output(vm, &sink),
            "\nFatal error: boom in t.php on line 7\nStack trace:\n#0 {main}\n"
        );
    }

    /// `E_PARSE` only ever comes from a `ParseError` through
    /// `zend_exception_error`, which clears the fatal backtrace; an
    /// `E_COMPILE_ERROR` from `zend_error_noreturn` records one.
    #[test]
    fn parse_level_compile_errors_carry_no_backtrace() {
        let (mut vm, sink) = vm_with_sink();
        vm.report_compile_error("oops", &ZStr::from("t.php"), 4, true);
        assert_eq!(vm.exit_status, 255);
        assert_eq!(
            output(vm, &sink),
            "\nParse error: oops in t.php on line 4\n"
        );
        let (mut vm, sink) = vm_with_sink();
        vm.report_compile_error("oops", &ZStr::from("t.php"), 4, false);
        assert_eq!(
            output(vm, &sink),
            "\nFatal error: oops in t.php on line 4\nStack trace:\n#0 {main}\n"
        );
    }

    fn lint(src: &str) -> (bool, i32, String) {
        let (mut vm, sink) = vm_with_sink();
        let ok = vm.lint_script(src.as_bytes(), &ZStr::from("t.php"));
        let status = vm.exit_status;
        (ok, status, output(vm, &sink))
    }

    /// `php_lint_script()` compiles the whole script, not just the parse,
    /// and its diagnostics are the request's (sapi/cli/tests/011, 024).
    #[test]
    fn lint_compiles_and_reports_like_a_request() {
        let (ok, status, out) = lint("<?php\necho 1;\n");
        assert!(ok);
        assert_eq!(status, 0);
        assert_eq!(out, "");

        let (ok, status, out) = lint("<?php\n$x = 1 +;\n");
        assert!(!ok);
        assert_eq!(status, 255);
        assert_eq!(
            out,
            "\nParse error: syntax error, unexpected token \";\" in t.php on line 2\n"
        );

        // Raised by the compiler, past the parser: `php -l` runs it too.
        let (ok, status, out) = lint("<?php\nfunction f() { return isset(f()); }\n");
        assert!(!ok);
        assert_eq!(status, 255);
        assert_eq!(
            out,
            "\nFatal error: Cannot use isset() on the result of an expression (you can use \"null !== expression\" instead) in t.php on line 2\nStack trace:\n#0 {main}\n"
        );

        let (ok, _, out) = lint("<?php\nstatic $a = 10;\nstatic $a = 11;\n");
        assert!(!ok);
        assert_eq!(
            out,
            "\nFatal error: Duplicate declaration of static variable $a in t.php on line 3\nStack trace:\n#0 {main}\n"
        );
    }
}

/// A fatal error was reported (`php_error_cb`): a `memory_limit`
/// violation still pending dies with the request unreported, as in
/// php-src, where the bailout leaves it behind (only the first fatal is
/// shown). Returns the exit status: 255, or 121
/// (`ZEND_MM_MAX_ALLOCS_EXIT_STATUS`) once the allocation chosen by
/// `ZEND_MM_MAX_ALLOCS` was reached, so a test driver tells the injected
/// failure from any other fatal.
fn fatal_ends_request() -> i32 {
    let _ = php_memlimit::take();
    if php_memlimit::max_allocs_hit() {
        121
    } else {
        255
    }
}
