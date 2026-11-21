// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The phprust engine: compiler and virtual machine.
//!
//! * [`compiler`] — compiles a `php_parser` AST into op arrays
//!   (`Zend/zend_compile.c`).
//! * [`Vm`] — the interpreter (`Zend/zend_execute.c`): call frames, classes,
//!   objects, exceptions, generators, closures, includes, error reporting
//!   and the builtin function library.
//!
//! The SAPIs (`crates/php-cli`, `crates/php-cli-server`) create a [`Vm`]
//! with [`Vm::with_extensions`],
//! set the command line arguments and run a script with
//! [`Vm::run_script`]. [`Vm::new`] creates the bare VM of the engine's own
//! tests.

pub use php_parser::ast;

mod attrs;
mod auto_globals;
mod builtins;
pub mod bytecode_cache;
mod call;
pub mod class;
mod classlink;
mod closure;
pub mod compiler;
mod configuration;
pub mod connection;
mod context;
mod discard;
mod enums;
mod errors;
mod exceptions;
mod exec;
pub mod ext;
mod fiber;
pub mod func;
pub mod gc;
mod generator;
pub mod include;
pub mod interrupt;
mod iter;
mod lazy;
mod log;
pub mod object;
mod observer;
pub mod opcode;
mod persistence;
mod pfa;
mod place;
mod props;
mod reflection;
pub mod reset;
pub mod rtcache;
mod special;
mod typecheck;
mod typed_refs;
pub mod types;
mod variance;
pub mod vm;
mod weakref;

pub use auto_globals::{AutoGlobalCallback, AUTO_GLOBAL_NAMES};
pub use exec::{oom_pending, try_reserve_memory};
pub use persistence::{PersistentDeclarations, PreloadInfo};
pub use vm::{ClassLoader, Throw, Vm, VmResult};

use php_runtime::{Value, ZArray, ZStr};
use php_support::ini::{Access, IniDirective, IniRegistry, ModifyHandler};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::rc::Rc;

/// The PHP version implemented (php-src master).
pub const PHP_VERSION: &str = "8.7.0-dev";
/// The Zend engine version.
pub const ZEND_VERSION: &str = "4.7.0-dev";
/// Extensions reported by `get_loaded_extensions()`.
pub const LOADED_EXTENSIONS: &[&str] = &[
    "Core",
    "date",
    "pcre",
    "random",
    "Reflection",
    "SPL",
    "standard",
    "hash",
];

/// Room to reserve in the function, class and constant tables before
/// start-up registers into them ([`Vm::new_core`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SymbolRoom {
    pub(crate) functions: usize,
    pub(crate) classes: usize,
    pub(crate) constants: usize,
}

impl SymbolRoom {
    /// What the VM's own builtin groups register (about 600 functions,
    /// 60 classes and 460 constants).
    pub(crate) const BUILTINS: SymbolRoom = SymbolRoom {
        functions: 640,
        classes: 64,
        constants: 480,
    };

    /// The builtins plus `n` extensions: the bundled ones average about
    /// 26 functions, 6 classes and 43 constants each (all 54 together
    /// end with 1 970 functions, 380 classes and 2 788 constants, which
    /// this reserves 4 096, 512 and 4 096 buckets for).
    pub(crate) fn with_extensions(n: usize) -> SymbolRoom {
        let b = SymbolRoom::BUILTINS;
        SymbolRoom {
            functions: b.functions + n * 28,
            classes: b.classes + n * 6,
            constants: b.constants + n * 44,
        }
    }
}

/// Options for creating a VM.
#[derive(Debug, Default, Clone)]
pub struct VmOptions {
    /// `-d name=value` settings (applied after the CLI overrides).
    pub ini: Vec<(String, String)>,
}

fn extra_directives() -> Vec<IniDirective> {
    let d = |n: &str, v: Option<&str>, h: ModifyHandler| {
        IniDirective::new(n, v, Access::ALL, h, "Core")
    };
    vec![
        d(
            "include_path",
            Some(".:/usr/share/php"),
            ModifyHandler::None,
        ),
        d(
            "zend.exception_string_param_max_len",
            Some("15"),
            ModifyHandler::ExceptionStringParamMaxLen,
        ),
        d("assert.active", Some("1"), ModifyHandler::Bool),
        d("assert.bail", Some("0"), ModifyHandler::Bool),
        d("assert.warning", Some("1"), ModifyHandler::Bool),
        d("assert.callback", None, ModifyHandler::None),
        d("assert.exception", Some("1"), ModifyHandler::Bool),
        d("date.timezone", Some("UTC"), ModifyHandler::None),
        d("arg_separator", Some("&"), ModifyHandler::None),
        d("highlight.bg", Some("#FFFFFF"), ModifyHandler::None),
        d("mbstring.language", Some("neutral"), ModifyHandler::None),
        d("zend.enable_gc", Some("1"), ModifyHandler::Bool),
    ]
}

impl Vm {
    /// Creates a bare VM: PHP's CLI defaults and the given ini settings,
    /// the VM's own builtins, and ext/standard's `count()` and `sizeof()`
    /// with the `COUNT_NORMAL`/`COUNT_RECURSIVE` constants, attributed to
    /// `standard` as the extension's registration attributes them, so
    /// that scripts run without extensions can count arrays.
    ///
    /// The SAPIs use [`Vm::with_extensions`] instead, which does not
    /// register these: there `count()` and `sizeof()` come only from the
    /// extension (php-ext-stdarr) that provides them.
    pub fn new(opts: &VmOptions, stdout: Box<dyn Write>) -> Vm {
        let mut vm = Vm::new_core(opts, stdout, SymbolRoom::BUILTINS);
        builtins::register_bare(&mut vm);
        vm
    }

    /// The VM before any extension registers (what [`Vm::with_extensions`]
    /// starts from): PHP's CLI defaults and the given ini settings, and
    /// the VM's own builtins.
    /// The symbol tables start with `room` for what start-up registers, so
    /// registration never rehashes them.
    pub(crate) fn new_core(opts: &VmOptions, stdout: Box<dyn Write>, room: SymbolRoom) -> Vm {
        let mut config: Vec<(String, Vec<u8>)> = php_support::ini::CLI_OVERRIDES
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.as_bytes().to_vec()))
            .collect();
        for (k, v) in &opts.ini {
            config.push((k.clone(), v.as_bytes().to_vec()));
        }
        let mut sink: Vec<php_support::Diagnostic> = Vec::new();
        let mut ini = IniRegistry::with_core(config.clone(), &mut sink);
        for d in extra_directives() {
            ini.register(d, &mut sink);
        }
        // A new executor (`init_executor()`): a fatal error or the shutdown
        // of an earlier request on this thread disabled destructors.
        object::set_destructors_enabled(true);
        let mut vm = Vm::with_ini(ini, stdout);
        vm.functions.reserve(room.functions);
        vm.classes.reserve(room.classes);
        vm.class_order.reserve(room.classes);
        vm.constants.reserve(room.constants);
        for n in ["error_reporting", "precision", "serialize_precision"] {
            vm.sync_ini(n);
        }
        // A recorded memory_limit violation raises the safe-point flag.
        crate::interrupt::install_memory_limit_hook();
        vm.startup_diagnostics = sink;
        // `zend_interned_strings_init()`: the empty and one-character
        // strings exist before any script runs, so their first use (e.g.
        // `(string) 0`) does not show in `memory_get_usage()` deltas.
        let _ = (ZStr::empty(), ZStr::from_byte(0));
        // `zend_objects_store_init()`: likewise for the object store.
        object::prewarm_store();
        builtins::register_all(&mut vm);
        vm
    }

    /// The executor state before start-up: every field at its initial
    /// value, with the ini registry and the output sink installed.
    fn with_ini(ini: IniRegistry, stdout: Box<dyn Write>) -> Vm {
        let empty_code = Rc::new(opcode::OpArray::default());
        Vm {
            frames: vm::FrameStack::new(),
            functions: class::NameMap::default(),
            classes: class::NameMap::default(),
            class_order: Vec::new(),
            constants: class::NameMap::default(),
            user_constants: Vec::new(),
            globals: Rc::new(RefCell::new(vm::VarStore::default())),
            ini,
            configuration: None,
            output: php_support::output::OutputStack::new(),
            stdout,
            error_reporting: 30719,
            error_handlers: Vec::new(),
            exception_handlers: Vec::new(),
            included: Vec::new(),
            included_set: HashSet::new(),
            autoloaders: Vec::new(),
            autoloading: HashSet::new(),
            attribute_call_sites: Vec::new(),
            exit_status: 0,
            shutdown_functions: Vec::new(),
            last_error: None,
            last_fatal_error_backtrace: None,
            pending_throw: None,
            output_handler_threw: false,
            script_path: ZStr::empty(),
            request_info: php_sapi::RequestInfo::default(),
            argv: Vec::new(),
            bound_decls: HashMap::new(),
            units: Vec::new(),
            precision: 14,
            serialize_precision: -1,
            in_shutdown: false,
            fast_shutdown: false,
            in_error_handler: 0,
            error_msg_html_ready: false,
            reporting_oom: false,
            ext: builtins::ExtState::default(),
            anon_counter: 0,
            depth: 0,
            once_flags: HashSet::new(),
            first_request_resource: 1,
            tick_functions: Vec::new(),
            interrupt_handlers: Vec::new(),
            tick_hooks: Vec::new(),
            user_ticks: Vec::new(),
            ticks_count: 0,
            fiber_switch_block: 0,
            timeout_seconds: 0,
            signals: crate::interrupt::this_thread(),
            execution_timer: None,
            headers_sent: false,
            response_mimetype: None,
            send_default_content_type: true,
            response_header_hook: None,
            send_headers_hook: None,
            timer_hook: None,
            rfc1867_callback: None,
            rfc1867_uploaded_files: Vec::new(),
            parse_body_hook: None,
            fibers: Vec::new(),
            loaded_extensions: Vec::new(),
            shutdown_hooks: Vec::new(),
            finalize_hooks: Vec::new(),
            startup_hooks: Vec::new(),
            module_startup_hooks: Vec::new(),
            ini_modify_hooks: std::collections::HashMap::new(),
            module_info: Vec::new(),
            dim_ctx: crate::ext::DimensionCtx::Read,
            prop_ctx: crate::ext::PropCtx::Read,
            ini_stage: crate::ext::IniStage::Runtime,
            magic_call_name: None,
            startup_diagnostics: Vec::new(),
            module_diagnostics: Vec::new(),
            sapi_module: std::rc::Rc::new(php_sapi::StderrLog),
            connection_status: connection::NORMAL,
            main_return: Value::Null,
            native_stack_base: 0,
            native_stack_size: 8 << 20,
            stack_limit_ini: None,
            call_bufs: crate::vm::CallBufs::prewarmed(&empty_code),
            output_handler_aliases: std::collections::HashMap::new(),
            output_handler_conflicts: std::collections::HashMap::new(),
            output_handler_hooks: std::collections::HashMap::new(),
            include_opener: None,
            source_filter: None,
            source_filter_probe: None,
            script_encoding_provider: None,
            parsed_sources: std::cell::RefCell::new(std::collections::HashMap::new()),
            builtin_deprecations: std::collections::HashMap::new(),
            builtin_attributes: std::collections::HashMap::new(),
            builtin_doc_comments: std::collections::HashMap::new(),
            const_deprecation_guard: std::collections::HashSet::new(),
            const_attributes: std::collections::HashMap::new(),
            const_files: std::collections::HashMap::new(),
            const_names: std::collections::HashMap::new(),
            deprecated_constants: std::collections::HashMap::new(),
            const_deprecation_filter: std::cell::RefCell::new((0, Vec::new())),
            ct_constants: None,
            compile_no_builtins: false,
            pending_call_dst: None,
            tape_throw: None,
            builtin_deprecation_reported: false,
            empty_code,
            empty_vars: Rc::new(RefCell::new(vm::VarStore::default())),
            prop_guards: HashSet::new(),
            observer: crate::observer::ObserverState::default(),
            user_opcode_handlers: Vec::new(),
        }
    }

    /// Re-reads cached ini values after a change.
    pub(crate) fn sync_ini(&mut self, name: &str) {
        match name {
            "error_reporting" => {
                self.error_reporting = match self.ini.get("error_reporting") {
                    Some(v) if !v.is_empty() => std::str::from_utf8(v)
                        .ok()
                        .and_then(|s| s.trim().parse::<i64>().ok())
                        .unwrap_or(0),
                    _ => 30719,
                };
            }
            "precision" => self.precision = self.ini.get_long("precision"),
            "max_execution_time" => {
                let t = self.ini.get_long("max_execution_time");
                self.set_time_limit(t);
            }
            "memory_limit" | "max_memory_limit" => {
                let limit = u64::try_from(self.ini.get_long("memory_limit")).unwrap_or(u64::MAX);
                php_memlimit::current().force_limit(php_memlimit::limit_from_ini(limit));
            }
            "serialize_precision" => {
                self.serialize_precision = self.ini.get_long("serialize_precision")
            }
            _ => {}
        }
    }

    /// Sets `$argv` and `$argc` for a CLI script (the SAPI fills the
    /// superglobals: `$_SERVER`, `$_ENV`, the request arrays).
    pub fn set_cli_args(&mut self, script: &str, args: &[Vec<u8>]) {
        let mut argv = vec![Value::Str(ZStr::from(script))];
        argv.extend(args.iter().map(|a| Value::Str(ZStr::from(a.as_slice()))));
        let argc = i64::try_from(argv.len()).unwrap_or(0);
        let argv_arr = Value::Array(ZArray::from_values(argv));
        let mut g = self.globals.borrow_mut();
        let mut set = |name: &str, v: Value| {
            let i = g.find_or_add(name.as_bytes()) as usize;
            g.slots[i] = Some(v);
        };
        set("argv", argv_arr);
        set("argc", Value::Int(argc));
        drop(g);
        self.script_path = ZStr::from(script);
    }

    /// Request startup: initializes output buffering, runs extension RINIT
    /// startup hooks, and starts memory limit accounting.
    pub fn request_startup(&mut self) {
        // `php_request_startup()`: `PG(connection_status) = PHP_CONNECTION_NORMAL`.
        self.connection_status = connection::NORMAL;
        php_support::output::request_startup(
            self,
            None,
            self.ini.get_long("output_buffering"),
            self.ini.get_bool("implicit_flush"),
        );
        for hook in self.startup_hooks.clone() {
            hook(self);
        }
        // memory_limit accounting starts with the request.
        let limit = u64::try_from(self.ini.get_long("memory_limit")).unwrap_or(u64::MAX);
        if php_memlimit::request_start(php_memlimit::limit_from_ini(limit)).is_err() {
            php_memlimit::current().force_limit(Some(php_memlimit::DEFAULT_LIMIT));
        }
        let t = self.ini.get_long("max_execution_time");
        self.set_time_limit(t);
    }

    /// Request shutdown: runs shutdown functions, destructors, extension
    /// RSHUTDOWN hooks, flushes output buffers, sends headers, and ends memory
    /// limit accounting; returns the exit status.
    pub fn request_shutdown(&mut self, r: Result<(), Throw>) -> i32 {
        self.shutdown(r)
    }

    /// Runs a script and performs request shutdown; returns the exit status.
    pub fn run_script(&mut self, src: &[u8], path: &str) -> i32 {
        self.request_startup();
        let p = ZStr::from(path);
        let mut r = Ok(());
        let prepend = self
            .ini
            .get("auto_prepend_file")
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        if !prepend.is_empty() {
            r = self.include_file(&String::from_utf8_lossy(&prepend));
        }
        if r.is_ok() {
            r = self.run_main(src, &p);
        }
        if r.is_ok() {
            let append = self
                .ini
                .get("auto_append_file")
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            if !append.is_empty() {
                r = self.include_file(&String::from_utf8_lossy(&append));
            }
        }
        self.shutdown(r)
    }

    /// Request shutdown: shutdown functions, destructors, output flush.
    /// The global symbol table, the objects still alive (emptied and
    /// marked destructed, so no destructor runs for them any more) and the
    /// cycles left that hold no object: what a request leaves behind once
    /// it ran no PHP code any more.
    fn release_request_state(&mut self) {
        let globals = std::mem::take(&mut *self.globals.borrow_mut());
        drop(globals);
        object::free_object_storage();
        self.gc_release_request_cycles();
    }

    /// Ends a request that could not end normally: a panic (a bug in Buzz)
    /// left the VM in an unknown state, so nothing of the request's PHP
    /// code may run any more, neither shutdown functions nor destructors,
    /// output handlers, user stream wrappers or filters, nor session save
    /// handlers. What the request holds is released all the same: the
    /// extensions' finalize hooks ([`ModuleBuilder::on_finalize`]) close
    /// their streams, sockets and connections without calling back into
    /// PHP, then the request's variables, objects and reference cycles are
    /// freed as at a normal end, and the memory accounting ends the
    /// request. Called once, in place of [`Vm::shutdown`]; the host then
    /// drops the VM and, as its thread-locals may be in an unknown state
    /// too, ends the thread.
    ///
    /// [`ModuleBuilder::on_finalize`]: crate::ext::ModuleBuilder::on_finalize
    pub fn finalize(&mut self) {
        object::set_destructors_enabled(false);
        self.frames.clear();
        self.shutdown_functions.clear();
        for hook in self.finalize_hooks.clone() {
            hook(self);
        }
        self.release_request_state();
        php_memlimit::request_end();
    }

    /// php-src's `fast_shutdown`: set when the process exits after this
    /// request and [`Vm::discard`] releases the VM. Request shutdown then
    /// does not destroy the global symbol table nor free the objects left
    /// (the process's end does), as php-src with its memory manager skips
    /// them. Off by default: a VM that is dropped or serves another
    /// request frees everything of the request, reference cycles included.
    pub fn set_fast_shutdown(&mut self, on: bool) {
        self.fast_shutdown = on;
    }

    pub fn shutdown(&mut self, r: Result<(), Throw>) -> i32 {
        let fatal = matches!(r, Err(Throw::Fatal));
        // `oom_after_main_script`: a violation recorded by the last
        // statement is still reported.
        if !fatal && php_memlimit::is_pending() {
            let _ = self.raise_pending_oom();
        }
        self.observer_unwind(0, r.as_ref().err());
        self.frames.clear();
        // A shutdown function may raise its own OOM.
        Vm::clear_pending_oom();
        // Shutdown functions.
        let mut i = 0;
        while i < self.shutdown_functions.len() {
            let (cb, args) = self.shutdown_functions[i].clone();
            i += 1;
            // `user_shutdown_function_call()` goes through
            // `zend_call_function()`, which marks the call
            // `ZEND_CALL_DYNAMIC` (`get_defined_vars()` refuses it).
            let r = match self.resolve_callable_for_call(&cb) {
                Ok(mut call) => {
                    call.dynamic = true;
                    call.args = args.into_iter().map(Some).collect();
                    self.call_pending(call)
                }
                Err(e) => Err(e),
            };
            match r {
                Ok(_) => {}
                Err(Throw::Exception(e)) => {
                    if self.uncaught_exception(e).is_err() {
                        break;
                    }
                }
                Err(Throw::Exit) => break,
                Err(Throw::Fatal | Throw::FiberSuspend) => break,
            }
            self.frames.clear();
        }
        let _ = fatal;
        // Destructors.
        if object::destructors_enabled() {
            self.in_shutdown = true;
            let _ = self.call_global_destructors();
            self.in_shutdown = false;
        }
        // `php_request_shutdown()`'s order: 3. flush all output buffers
        // (`php_output_end_all()`), 5. the extensions' RSHUTDOWN, 6. shut
        // the output layer down (`php_output_deactivate()`). A handler a
        // module started (`output_handler=ob_gzhandler`) still has its
        // module's request state when it is flushed.
        php_support::output::end_all(self);
        for h in self.shutdown_hooks.clone() {
            h(self);
        }
        php_support::output::deactivate(self);
        object::set_destructors_enabled(false);
        // `sapi_deactivate`: headers go out (and header_register_callback
        // runs) even when the script produced no output.
        crate::builtins::headers::send_headers(self);
        self.flush_stdout();
        // `zend_deactivate()` (step 10), `zend_shutdown_executor_values()`:
        // unless the process exits next (`fast_shutdown`), the global
        // symbol table is destroyed, then the objects still alive are
        // freed, the ones only a reference cycle keeps alive included, and
        // the cycles without an object are collected. A host running more
        // requests (a server, a reused VM) is left with nothing of this
        // request; php-src's request heap frees what php-src leaves.
        if !self.fast_shutdown {
            self.release_request_state();
        }
        php_memlimit::request_end();
        self.exit_status
    }

    fn call_global_destructors(&mut self) -> Result<(), Throw> {
        // Objects released on the way here (the uncaught exception and its
        // trace, the unwound frames).
        if let Err(e) = self.run_pending_destructors() {
            return self.destructor_error(e);
        }
        // Globals holding the only reference, in reverse order.
        loop {
            let n = self.globals.borrow().slots.len();
            let mut removed = false;
            for i in (0..n).rev() {
                let take = {
                    let g = self.globals.borrow();
                    match &g.slots[i] {
                        Some(Value::Object(o)) => o.refcount() == 1,
                        _ => false,
                    }
                };
                if take {
                    let v = self.globals.borrow_mut().slots[i].take();
                    drop(v);
                    removed = true;
                    if let Err(e) = self.run_pending_destructors() {
                        return self.destructor_error(e);
                    }
                }
            }
            if !removed {
                break;
            }
        }
        // `zend_objects_store_call_destructors`: the remaining objects in
        // handle order, including the ones their destructors create (which
        // take fresh handles at the end of the store meanwhile).
        object::set_handle_no_reuse(true);
        let r = self.call_store_destructors();
        object::set_handle_no_reuse(false);
        r
    }

    fn call_store_destructors(&mut self) -> Result<(), Throw> {
        // The store is not freed here: an object whose destructor dropped
        // its last reference stays allocated (`GC_DELREF`, no release).
        let mut keep = Vec::new();
        let mut i = 0;
        while i < object::store_len() {
            // The object's own handle (`GC_ADDREF(obj)`), not a second
            // `Rc`: `$this` in the destructor is identical (`===`, the
            // recursion guards of `var_dump`) to the other references.
            let slot = object::live_obj_at(i);
            i += 1;
            let Some(o) = slot else {
                continue;
            };
            let d = object::data(&o);
            if d.flags.get() & object::oflags::DESTRUCTED != 0 || !d.class.has_destructor() {
                continue;
            }
            d.flags.set(d.flags.get() | object::oflags::DESTRUCTED);
            if let Err(e) = self.call_destructor(&o) {
                return self.destructor_error(e);
            }
            // `GC_DELREF` without the release: the handle `o` took
            // (`GC_ADDREF(obj)` around `dtor_obj`, the only extra count
            // the destructor sees) is held until the loop is done, so an
            // object whose destructor dropped its last reference is not
            // freed by it.
            keep.push(o);
            if let Err(e) = self.run_pending_destructors() {
                return self.destructor_error(e);
            }
        }
        Ok(())
    }

    fn destructor_error(&mut self, e: Throw) -> Result<(), Throw> {
        match e {
            Throw::Exception(ex) => {
                let _ = self.uncaught_exception(ex);
                object::set_destructors_enabled(false);
                Err(Throw::Fatal)
            }
            other => Err(other),
        }
    }

    /// Checks file syntax with default settings (legacy embedding convenience).
    /// Configured hosts, including CLI lint, use [`Self::parse_source`] instead.
    pub fn lint(src: &[u8]) -> Result<(), (String, u32, bool)> {
        let opts = php_parser::ParseOptions {
            skip_shebang: true,
            ..Default::default()
        };
        match php_parser::parse(src, &opts) {
            Ok(_) => Ok(()),
            Err(e) => Err((e.message, e.line, e.kind == php_parser::ErrorKind::Parse)),
        }
    }
}

/// Core constants usable in INI values before any extension starts
/// (`zend_register_standard_constants`).
fn startup_ini_constant(name: &[u8]) -> Option<Vec<u8>> {
    let v: i64 = match name {
        b"E_ERROR" => 1,
        b"E_WARNING" => 2,
        b"E_PARSE" => 4,
        b"E_NOTICE" => 8,
        b"E_CORE_ERROR" => 16,
        b"E_CORE_WARNING" => 32,
        b"E_COMPILE_ERROR" => 64,
        b"E_COMPILE_WARNING" => 128,
        b"E_USER_ERROR" => 256,
        b"E_USER_WARNING" => 512,
        b"E_USER_NOTICE" => 1024,
        b"E_STRICT" => 2048,
        b"E_RECOVERABLE_ERROR" => 4096,
        b"E_DEPRECATED" => 8192,
        b"E_USER_DEPRECATED" => 16384,
        b"E_ALL" => 30719,
        b"PHP_INT_MAX" => i64::MAX,
        b"PHP_INT_SIZE" => 8,
        _ => return None,
    };
    Some(v.to_string().into_bytes())
}

struct DefineCollector(Vec<(String, String)>);

impl php_support::ini::IniParserCallback for DefineCollector {
    fn section(&mut self, _name: &[u8]) {}
    fn entry(&mut self, key: &[u8], value: Option<php_support::ini::IniScalar>) {
        let v = value.map(|v| v.to_bytes()).unwrap_or_default();
        self.0.push((
            String::from_utf8_lossy(key).into_owned(),
            String::from_utf8_lossy(&v).into_owned(),
        ));
    }
    /// `name[offset]=value` keeps its offset in the name (`ary[a]`) for
    /// the SAPI's configuration hash (`ZEND_INI_PARSER_POP_ENTRY`).
    fn pop_entry(&mut self, key: &[u8], value: php_support::ini::IniScalar, offset: &[u8]) {
        let mut name = key.to_vec();
        name.push(b'[');
        name.extend_from_slice(offset);
        name.push(b']');
        self.entry(&name, Some(value));
    }
}

/// The CLI's `-d name=value` (`php_cli.c`): a value starting with a
/// character that is neither alphanumeric nor a quote is quoted, then the
/// line is parsed as INI (comments, constants such as `E_ALL`,
/// expressions, quoting).
pub fn parse_cli_define(arg: &[u8]) -> Vec<(String, String)> {
    let mut line = Vec::with_capacity(arg.len() + 4);
    match arg.iter().position(|&c| c == b'=') {
        Some(p) => {
            let val = &arg[p + 1..];
            match val.first() {
                Some(&c) if !c.is_ascii_alphanumeric() && c != b'"' && c != b'\'' => {
                    line.extend_from_slice(&arg[..=p]);
                    line.push(b'"');
                    line.extend_from_slice(val);
                    line.push(b'"');
                }
                _ => line.extend_from_slice(arg),
            }
        }
        None => {
            line.extend_from_slice(arg);
            line.extend_from_slice(b"=1");
        }
    }
    line.push(b'\n');
    let mut hooks = php_support::ini::FnHooks {
        constant: startup_ini_constant,
        variable: |n: &[u8]| {
            std::env::var_os(String::from_utf8_lossy(n).as_ref())
                .map(|v| v.to_string_lossy().into_owned().into_bytes())
        },
    };
    let mut out = DefineCollector(Vec::new());
    let _ = php_support::ini::parse_ini(
        &line,
        "Unknown",
        php_support::ini::ScannerMode::Normal,
        &mut hooks,
        &mut out,
    );
    out.0
}

#[cfg(test)]
mod cli_define_tests {
    use super::parse_cli_define;

    fn one(s: &str) -> (String, String) {
        parse_cli_define(s.as_bytes())
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    #[test]
    fn defines() {
        assert_eq!(
            one("error_reporting=E_ALL"),
            ("error_reporting".into(), "30719".into())
        );
        assert_eq!(
            one("date.timezone=UTC;"),
            ("date.timezone".into(), "UTC".into())
        );
        assert_eq!(one("display_errors"), ("display_errors".into(), "1".into()));
        assert_eq!(one("x=\"a b\""), ("x".into(), "a b".into()));
        assert_eq!(
            one("error_reporting=E_ALL & ~E_DEPRECATED"),
            ("error_reporting".into(), "22527".into())
        );
        assert_eq!(one("ary[a]=1"), ("ary[a]".into(), "1".into()));
        assert_eq!(one("ary[]=x"), ("ary[]".into(), "x".into()));
    }
}
