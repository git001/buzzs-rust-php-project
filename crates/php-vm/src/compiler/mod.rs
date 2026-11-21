// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! AST → opcode compiler (`Zend/zend_compile.c`).

pub(crate) mod attrs;
mod constexpr;
mod decl;
#[cfg(test)]
mod decl_tree_tests;
mod expr;
#[cfg(test)]
mod fbc_tests;
#[cfg(test)]
mod fold_tests;
#[cfg(test)]
mod loop_layout_tests;
pub mod optimizer;
mod specialise;
mod stack;
#[cfg(test)]
mod static_vars_tests;
mod stmt;

pub use stack::CompileStackLimit;

use crate::ast::{self, Name, NameKind};
use crate::class::{AttributeInfo, ClassDecl, ConstExpr};
use crate::func::UserFunction;
use crate::opcode::{
    ClassSpec, DeclTable, Label, Op, OpArray, Operand, Place, PlaceBase, PlaceId, TryEntry,
};
use crate::types::TypeDecl;
use php_runtime::{Value, ZStr};
use std::collections::HashMap;
use std::rc::Rc;

/// A fatal error raised while compiling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileError {
    /// The message, as PHP prints it.
    pub message: String,
    /// The line.
    pub line: u32,
    /// `E_COMPILE_ERROR` (false) or `E_ERROR`-class… kept for display.
    pub is_parse: bool,
    /// Compile-time warnings accumulated before the fatal error.
    pub warnings: Vec<(i64, String, u32)>,
}

impl CompileError {
    fn new(message: impl Into<String>, line: u32, warnings: Vec<(i64, String, u32)>) -> Self {
        CompileError {
            message: message.into(),
            line,
            is_parse: false,
            warnings,
        }
    }
}

/// Result type of compiler functions.
pub type CResult<T> = Result<T, CompileError>;

/// A compiled file (or `eval` string).
#[derive(Debug)]
pub struct CompiledUnit {
    /// The top-level code.
    pub main: Rc<UserFunction>,
    /// Functions to declare before execution (unconditional top-level).
    pub hoisted_functions: Vec<u32>,
    /// Classes to try to declare before execution (top-level).
    pub hoisted_classes: Vec<u32>,
    /// The unit's declaration table: the main op array's declarations
    /// (which `hoisted_functions` and `hoisted_classes` index), followed in
    /// compile order by every declaration nested deeper in the unit.
    pub decls: Rc<DeclTable>,
    /// Compile-time warnings (level bits, message, line) to emit before
    /// execution.
    pub warnings: Vec<(i64, String, u32)>,
    /// The anonymous class counter after this unit (see
    /// [`CompileOptions::anon_base`]).
    pub anon_end: u32,
    /// What compiling the unit did besides producing it, recorded so a
    /// cached unit can repeat it (`zend_persistent_script`'s
    /// `ping_auto_globals_mask` and friends). Filled by
    /// `Vm::compile_source_default`; empty for `eval` and fresh units.
    pub meta: UnitMeta,
}

/// The side effects of compiling a file that a cache hit has to repeat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitMeta {
    /// The anonymous class counter the unit was compiled from
    /// ([`CompileOptions::anon_base`]); anonymous class names embed it.
    pub anon_base: u32,
    /// `__COMPILER_HALT_OFFSET__` of the file, if it calls
    /// `__halt_compiler()`.
    pub halt_offset: Option<i64>,
    /// The auto-globals (`$_SERVER`, `$_ENV`, ... by registration index)
    /// the code names, whose just-in-time callbacks compiling armed.
    pub auto_globals: Vec<u8>,
}

/// The global constants `zend_try_ct_eval_const` (`Zend/zend_compile.c`)
/// may substitute while compiling: the persistent internal constants with
/// scalar values that are not deprecated (`can_ct_eval_const`), by their
/// case-sensitive name. Shared by every compilation of a request.
#[derive(Clone, Default)]
pub struct CtConstants(Rc<crate::class::NameMap<Value>>);

impl CtConstants {
    /// Wraps a name → value table.
    pub(crate) fn new(constants: crate::class::NameMap<Value>) -> Self {
        Self(Rc::new(constants))
    }

    /// The value of the constant `name`, if it may be substituted.
    pub fn get(&self, name: &[u8]) -> Option<&Value> {
        self.0.get(name)
    }

    /// The number of substitutable constants.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no constant may be substituted.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A hash of every substitutable constant's name and value: code
    /// compiled against one table may only be reused (from the bytecode
    /// cache) where the table is the same. Memoized per table.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        thread_local! {
            static MEMO: std::cell::Cell<(usize, u64)> = const { std::cell::Cell::new((0, 0)) };
        }
        let key = Rc::as_ptr(&self.0) as usize;
        let (memo_key, memo) = MEMO.with(std::cell::Cell::get);
        if memo_key == key {
            return memo;
        }
        let mut entries: Vec<(&[u8], &Value)> =
            self.0.iter().map(|(k, v)| (k.as_bytes(), v)).collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (name, value) in entries {
            name.hash(&mut h);
            match value {
                Value::Null => 0_u8.hash(&mut h),
                Value::Bool(b) => (1_u8, *b).hash(&mut h),
                Value::Int(i) => (2_u8, *i).hash(&mut h),
                Value::Float(f) => (3_u8, f.to_bits()).hash(&mut h),
                Value::Str(s) => (4_u8, s.as_bytes()).hash(&mut h),
                // Only scalars are substitutable (`can_ct_eval_const`).
                _ => 5_u8.hash(&mut h),
            }
        }
        let fp = h.finish();
        MEMO.with(|m| m.set((key, fp)));
        fp
    }
}

impl std::fmt::Debug for CtConstants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CtConstants({} entries)", self.0.len())
    }
}

/// The builtins of `zend_try_compile_special_func_ex()` that compile to a
/// dedicated opcode or a constant here (bit order of
/// [`CompileOptions::special_builtins`]). Aliases (`is_integer`,
/// `doubleval`, `key_exists`, ...) are not special in php-src.
pub const SPECIAL_BUILTINS: &[&str] = &[
    "strlen",
    "count",
    "sizeof",
    "is_null",
    "is_bool",
    "is_int",
    "is_float",
    "is_string",
    "is_array",
    "is_object",
    "is_resource",
    "is_scalar",
    "boolval",
    "intval",
    "floatval",
    "strval",
    "gettype",
    "get_class",
    "get_called_class",
    "defined",
    "in_array",
    "array_key_exists",
    "chr",
    "ord",
];

/// The internal functions with `@frameless-function` handlers in php-src
/// master's stubs (`Zend/zend_builtin_functions.stub.php`,
/// `ext/standard/basic_functions.stub.php`, `ext/pcre/php_pcre.stub.php`),
/// with the argument counts they have a handler for
/// (`zend_frameless_function_info.num_args`).
const FRAMELESS_FUNCTIONS: &[(&str, &[u32])] = &[
    ("clamp", &[3]),
    ("class_exists", &[1, 2]),
    ("dechex", &[1]),
    ("dirname", &[1, 2]),
    ("implode", &[1, 2]),
    ("in_array", &[2, 3]),
    ("is_numeric", &[1]),
    ("ltrim", &[1, 2]),
    ("max", &[2]),
    ("min", &[2]),
    ("preg_match", &[2]),
    ("preg_replace", &[3]),
    ("property_exists", &[2]),
    ("rtrim", &[1, 2]),
    ("str_contains", &[2]),
    ("str_ends_with", &[2]),
    ("str_replace", &[3]),
    ("str_starts_with", &[2]),
    ("strpos", &[2, 3]),
    ("strstr", &[2, 3]),
    ("strtolower", &[1]),
    ("strtoupper", &[1]),
    ("strtr", &[2, 3]),
    ("substr", &[2, 3]),
    ("trim", &[1, 2]),
];

/// The frameless handler arities of internal function `lcname`.
pub(crate) fn frameless_arities(lcname: &[u8]) -> Option<&'static [u32]> {
    FRAMELESS_FUNCTIONS
        .iter()
        .find(|(name, _)| name.as_bytes() == lcname)
        .map(|&(_, arities)| arities)
}

/// The [`CompileOptions::special_builtins`] bit of lower-case `name`.
pub fn special_builtin_bit(name: &[u8]) -> Option<u32> {
    SPECIAL_BUILTINS
        .iter()
        .position(|n| n.as_bytes() == name)
        .and_then(|i| u32::try_from(i).ok())
        .map(|i| 1 << i)
}

/// Options for [`compile`].
#[derive(Clone, Debug, Default)]
pub struct CompileOptions {
    /// The file name used by `__FILE__` and diagnostics.
    pub filename: ZStr,
    /// Compiling `eval()`'d code.
    pub is_eval: bool,
    /// Class scope name for eval inside a class (for `__CLASS__`).
    pub eval_class: Option<ZStr>,
    /// `zend.assertions` value at compile time (`-1` drops asserts).
    pub assertions: i64,
    /// Parent class for `parent` in eval'd class-scope code.
    pub eval_parent: Option<ZStr>,
    /// Name given to the top-level closure (partial applications).
    pub closure_name: Option<ZStr>,
    /// Initial `strict_types`.
    pub strict_types: bool,
    /// Whether `sprintf` is currently registered as an internal function.
    pub sprintf_builtin: bool,
    /// Whether `array_map` is currently registered as an internal function
    /// (`zend_compile_func_array_map()` needs its arginfo).
    pub array_map_builtin: bool,
    /// Whether `strlen` is currently registered as an internal function
    /// (`zend_try_compile_special_func()` only applies to internal
    /// functions found at compile time).
    pub strlen_builtin: bool,
    /// The builtins `zend_try_compile_special_func()` may compile to
    /// dedicated opcodes: bit `i` is set when [`SPECIAL_BUILTINS`]`[i]` is
    /// currently registered as an internal function
    /// (`fbc->type == ZEND_INTERNAL_FUNCTION`). Zero (the default) compiles
    /// ordinary calls.
    pub special_builtins: u32,
    /// The request-wide anonymous class counter at the start of this
    /// compilation (`CG(rtd_key_counter)`), so recompiling a file or eval
    /// string names its anonymous classes afresh.
    pub anon_base: u32,
    /// Internal constants substituted at compile time
    /// (`zend_try_ct_eval_const`); `None` substitutes only `true`, `false`
    /// and `null`.
    pub ct_constants: Option<CtConstants>,
    /// The C stack model of compile-time `zend_check_stack_limit()`;
    /// `None` disables the check.
    pub stack_limit: Option<CompileStackLimit>,
    /// Zend opcodes with a `zend_set_user_opcode_handler()` hook: each is
    /// preceded by an `Op::UserOpcode`.
    pub user_opcodes: Vec<&'static str>,
    /// `opcache.optimization_level` bits (`0` = disabled).
    pub optimization_level: u32,
    /// Drop the doc comments, as `zend_persist` does for a script opcache
    /// caches under `opcache.save_comments=0`.
    pub strip_doc_comments: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FnKind {
    Main,
    Function,
    Method,
    Closure,
    ConstExpr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopKind {
    Loop,
    Switch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cleanup {
    None,
    FeFree(u32),
    FreeTmp(u32),
}

#[derive(Debug)]
pub(crate) struct LoopInfo {
    pub kind: LoopKind,
    pub break_patches: Vec<usize>,
    pub continue_patches: Vec<usize>,
    pub cleanup: Cleanup,
    pub finally_depth: usize,
    /// Finally-body nesting (`FnCtx::in_finally`) the loop was opened at:
    /// a `break`/`continue` compiled deeper jumps out of a finally block.
    pub in_finally: u32,
}

/// An unwinding op emitted before a `goto` (`zend_compile_goto` calls
/// `zend_handle_loops_and_finally`); the ones the jump does not leave are
/// turned into NOPs once the label is known (`zend_resolve_goto_label`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GotoUnwind {
    /// Frees the variable of the loop with this id.
    Loop(usize),
    /// Calls the finally block with this id.
    Finally(u32),
}

/// A `goto` whose label is resolved when the function is finished.
#[derive(Debug)]
pub(crate) struct PendingGoto {
    /// The `Jmp` op.
    pub at: usize,
    pub label: Vec<u8>,
    pub line: u32,
    /// Ids of the loops enclosing the `goto`.
    pub loops: Vec<usize>,
    /// The unwinding ops emitted before the jump, innermost first.
    pub unwind: Vec<(usize, GotoUnwind)>,
}

/// Class context while compiling methods.
#[derive(Clone, Debug)]
pub(crate) struct ClassCtx {
    pub name: ZStr,
    pub parent: Option<ZStr>,
    pub is_trait: bool,
    #[allow(dead_code)]
    pub is_interface: bool,
    pub is_anon: bool,
}

/// Per-function compilation state.
#[derive(Debug)]
pub(crate) struct FnCtx {
    pub kind: FnKind,
    pub ops: Vec<Op>,
    pub lines: Vec<u32>,
    pub literals: Vec<Value>,
    /// The literal slot of each string literal, keyed by its interned
    /// instance (looked up by bytes).
    lit_strs: crate::class::NameMap<u32>,
    pub places: Vec<Place>,
    pub cv_names: Vec<ZStr>,
    cv_map: HashMap<Vec<u8>, u32>,
    pub num_tmps: u32,
    pub num_iters: u32,
    pub num_finally: u32,
    pub num_statics: u32,
    /// See [`OpArray::static_vars`].
    pub static_vars: Vec<crate::opcode::StaticVarDecl>,
    pub try_table: Vec<(TryEntry, Label)>,
    pub loops: Vec<LoopInfo>,
    pub finally_stack: Vec<u32>,
    pub labels: HashMap<Vec<u8>, (Label, Vec<usize>)>,
    pub gotos: Vec<PendingGoto>,
    pub loop_ids: Vec<usize>,
    /// `FE_RESET` op of each foreach iterator (exceptions thrown while a
    /// `return` frees the loop are dispatched from the loop's position).
    pub iter_starts: HashMap<u32, u32>,
    next_loop_id: usize,
    pub line: u32,
    pub is_generator: bool,
    pub returns_ref: bool,
    pub has_return_type: bool,
    pub return_type: Option<TypeDecl>,
    pub func_name: ZStr,
    pub method_name: ZStr,
    pub is_static: bool,
    /// Short-circuit jumps of the current nullsafe chain.
    pub sc_jumps: Vec<usize>,
    /// Compiling an `isset()`/`empty()`/`??` operand: nullsafe bases are
    /// fetched in `BP_VAR_IS` mode.
    pub sc_quiet: bool,
    /// See [`OpArray::live_tmps`].
    pub live_tmps: Vec<crate::opcode::LiveTmp>,
    pub in_finally: u32,
    /// Pending `FastCall` patches: (finally id, op index).
    pub fast_calls: Vec<(u32, usize)>,
    /// `CG(context).has_assigned_to_http_response_header`: once this op
    /// array writes `$http_response_header` (or takes it as a parameter),
    /// later reads no longer raise the deprecation.
    pub http_response_header_assigned: bool,
    /// `CG(context).in_jmp_frameless_branch`: compiling the arguments of a
    /// namespaced call that has a frameless branch, so calls nested in them
    /// get none (`zend_compile_ns_call`).
    pub in_frameless_branch: bool,
    /// The functions, closures, classes and constant attributes declared in
    /// this op array (php-src's `dynamic_func_defs`): what its declaration
    /// ops index ([`OpArray::decls`]). `None` until the first declaration;
    /// the main op array's is the unit's table ([`Compiler::decls`]).
    pub decls: Option<Rc<DeclTable>>,
    /// The CVs bound before the first op (parameters, then closure `use`
    /// variables), with whether each is bound by reference (for the
    /// optimizer).
    pub bound_cvs: Vec<(u32, bool)>,
    /// The by-value, non-variadic parameters whose declared type is
    /// scalar-only (`int`, `?string`, `bool|float`, ...): what they are
    /// bound to has no destructor (for the optimizer).
    pub scalar_params: Vec<u32>,
}

impl FnCtx {
    pub(crate) fn pending_fast_calls(&mut self) -> &mut Vec<(u32, usize)> {
        &mut self.fast_calls
    }

    pub(crate) fn new(kind: FnKind) -> Self {
        FnCtx {
            kind,
            ops: Vec::new(),
            lines: Vec::new(),
            literals: Vec::new(),
            lit_strs: crate::class::NameMap::default(),
            places: Vec::new(),
            cv_names: Vec::new(),
            cv_map: HashMap::new(),
            num_tmps: 0,
            num_iters: 0,
            num_finally: 0,
            num_statics: 0,
            static_vars: Vec::new(),
            try_table: Vec::new(),
            loops: Vec::new(),
            finally_stack: Vec::new(),
            labels: HashMap::new(),
            gotos: Vec::new(),
            loop_ids: Vec::new(),
            iter_starts: HashMap::new(),
            next_loop_id: 0,
            line: 1,
            is_generator: false,
            returns_ref: false,
            has_return_type: false,
            return_type: None,
            func_name: ZStr::empty(),
            method_name: ZStr::empty(),
            is_static: false,
            sc_jumps: Vec::new(),
            sc_quiet: false,
            live_tmps: Vec::new(),
            in_finally: 0,
            fast_calls: Vec::new(),
            http_response_header_assigned: false,
            in_frameless_branch: false,
            decls: None,
            bound_cvs: Vec::new(),
            scalar_params: Vec::new(),
        }
    }
}

/// The compiler for one file.
pub(crate) struct Compiler<'f> {
    pub filename: ZStr,
    pub namespace: Vec<u8>,
    pub class_uses: HashMap<Vec<u8>, Vec<u8>>,
    pub fn_uses: HashMap<Vec<u8>, Vec<u8>>,
    pub const_uses: HashMap<Vec<u8>, Vec<u8>>,
    /// `FC(seen_symbols)`: lower-case class/function names and constant
    /// names declared so far in the file, with their `ZEND_SYMBOL_*` kinds.
    pub seen_symbols: HashMap<Vec<u8>, u8>,
    pub strict_types: bool,
    /// `FC(declarables).ticks`: N of the active `declare(ticks=N)`.
    pub(crate) ticks: u32,
    /// The unit's declaration table: the main op array's declarations,
    /// which its ops and [`CompiledUnit::hoisted_functions`] /
    /// [`CompiledUnit::hoisted_classes`] index, followed (in compile order)
    /// by every declaration nested deeper in the unit, so that looking a
    /// declaration up through the unit finds them all. Only the main op
    /// array refers to it: a nested op array's ops index its own table
    /// ([`FnCtx::decls`]), so no declaration's code holds a table that holds
    /// the declaration (an `Rc` cycle that kept every user function and
    /// class of a request alive after the VM was dropped).
    pub decls: Rc<DeclTable>,
    /// The table of the op arrays that declare nothing.
    empty_decls: Rc<DeclTable>,
    pub fns: Vec<FnCtx>,
    pub classes: Vec<ClassCtx>,
    pub hoisted_functions: Vec<u32>,
    pub hoisted_classes: Vec<u32>,
    pub warnings: Vec<(i64, String, u32)>,
    pub anon_counter: u32,
    pub opts: CompileOptions,
    /// Names of functions declared in this file (for redeclare errors).
    pub declared_functions: HashMap<Vec<u8>, u32>,
    /// `CG(function_table)` entries finalized so far: the top-level
    /// functions of this file whose compilation finished, by lower-case
    /// name. `zend_compile_args` knows their arginfo (`fbc`).
    pub known_functions: HashMap<Vec<u8>, FunctionInfo>,
    /// The rest of `CG(function_table)`: the functions declared when the
    /// compilation started (`None`: only this file's are known).
    pub functions: Option<&'f dyn FunctionTable>,
    pub source: Rc<[u8]>,
    /// Original source offsets cannot recover multibyte semantic text.
    pub multibyte: bool,
    /// Property name while compiling a property hook (`__PROPERTY__`).
    pub hook_prop: Option<Vec<u8>>,
    pub closure_depth: u32,
    /// Compiling a parameter default: a constant expression whose
    /// `self`/`parent` (in `new` and `X::C`) are only checked when it is
    /// evaluated, as `zend_compile_const_expr_*` do not call
    /// `zend_ensure_valid_class_fetch_type()`.
    pub in_const_default: bool,
    /// Compiling the value of a top-level `const` statement, which PHP
    /// evaluates as a constant-expression AST (`zend_compile_const_decl`).
    pub in_const_decl: bool,
    /// `fns.len()` while a global `const` value or a parameter default is
    /// compiled inline (0 otherwise): these are constant expressions in
    /// php-src, evaluated by `zend_ast_evaluate_ex`.
    pub inline_const_depth: usize,
    /// Compiling the value of a top-level `const` statement, which runs
    /// inline rather than as a constant-expression op array.
    pub in_global_const: bool,
    /// While compiling a function's or method's attributes: its display
    /// name (`foo()`, `C::m()`), which names the closures in the attribute
    /// arguments as `CG(active_op_array)` is already the function.
    pub attr_owner: Option<String>,
    /// `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION |
    /// ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION`, set by
    /// `zend_compile_params` around a parameter default so that
    /// `ReflectionParameter::isDefaultValueConstant()` still sees the
    /// constant.
    pub no_const_subst: bool,
    /// Compile-time `zend_check_stack_limit()` state.
    pub(crate) stack: stack::StackModel,
}

/// What `zend_compile_args` reads of a function it finds in
/// `CG(function_table)` at compile time (its `fbc`): how each argument is
/// sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FunctionInfo {
    /// `fbc->type == ZEND_INTERNAL_FUNCTION`.
    pub internal: bool,
    /// `fbc->common.required_num_args`.
    pub required: u32,
    /// The declared parameters, a variadic one included.
    pub num_params: u32,
    /// The last parameter is variadic (`ZEND_ACC_VARIADIC`).
    pub variadic: bool,
    /// Bit `i`: parameter `i` is sent by reference (`ZEND_SEND_BY_REF` or
    /// `ZEND_SEND_PREFER_REF`); parameters past the 64th count as by value.
    pub by_ref: u64,
}

impl FunctionInfo {
    /// The argument send modes of `func`.
    #[must_use]
    pub fn of(func: &crate::func::Func) -> Self {
        Self::from_flags(
            !func.is_user(),
            func.required(),
            func.params().iter().map(|p| (p.by_ref, p.variadic)),
        )
    }

    /// From each declared parameter's (by reference, variadic) flags.
    pub(crate) fn from_flags(
        internal: bool,
        required: u32,
        params: impl Iterator<Item = (bool, bool)>,
    ) -> Self {
        let mut info = FunctionInfo {
            internal,
            required,
            ..Self::default()
        };
        for (i, (by_ref, variadic)) in params.enumerate() {
            if by_ref && i < 64 {
                info.by_ref |= 1u64 << i;
            }
            info.variadic = variadic;
            info.num_params = u32::try_from(i + 1).unwrap_or(u32::MAX);
        }
        info
    }

    /// `ARG_SHOULD_BE_SENT_BY_REF(fbc, i + 1)`: past the declared
    /// parameters, the variadic one's mode.
    #[must_use]
    pub fn sends_by_ref(&self, i: usize) -> bool {
        let declared = self.num_params as usize;
        let i = if i < declared {
            i
        } else if self.variadic {
            declared - 1
        } else {
            return false;
        };
        i < 64 && self.by_ref & (1u64 << i) != 0
    }
}

/// `CG(function_table)` when a compilation starts: the functions declared
/// so far (the internal ones and those of the code that already ran), which
/// `zend_compile_call` looks up by lower-case name to know a call's `fbc`.
pub trait FunctionTable {
    /// The function `lcname`, if one is declared.
    fn find(&self, lcname: &[u8]) -> Option<FunctionInfo>;
}

impl<F: Fn(&[u8]) -> Option<FunctionInfo>> FunctionTable for F {
    fn find(&self, lcname: &[u8]) -> Option<FunctionInfo> {
        self(lcname)
    }
}

/// Compiles a parsed program.
pub fn compile(
    program: &ast::Program,
    source: Rc<[u8]>,
    opts: CompileOptions,
) -> CResult<CompiledUnit> {
    let mut c = Compiler::start(program, source, opts, None);
    c.compile_top_statements(&program.statements)?;
    c.finish_unit()
}

/// [`compile`] knowing the functions declared when it starts: with a
/// known `fbc`, `zend_compile_args` compiles an argument for a by-value
/// parameter for reading (`f($a[])` is then "Cannot use [] for reading"
/// at compile time, a run-time `Error` otherwise).
pub fn compile_with_functions(
    program: &ast::Program,
    source: Rc<[u8]>,
    opts: CompileOptions,
    functions: &dyn FunctionTable,
) -> CResult<CompiledUnit> {
    let mut c = Compiler::start(program, source, opts, Some(functions));
    c.compile_top_statements(&program.statements)?;
    c.finish_unit()
}

/// Compiles a parsed program it consumes, releasing the AST of each
/// top-level statement as soon as it is compiled. php-src frees a file's
/// AST arena only after `zend_compile_top_stmt` is done, but its arena
/// nodes are a fraction of the size of ours: dropping statements as we go
/// keeps the AST of a large file and its op array from being alive in full
/// together (`Zend/tests/runtime_compile_time_binary_operands.phpt`
/// includes 11 MB of generated code under `memory_limit=256M`).
pub fn compile_program(
    program: ast::Program,
    source: Rc<[u8]>,
    opts: CompileOptions,
) -> CResult<CompiledUnit> {
    compile_program_inner(program, source, opts, None)
}

/// [`compile_program`] knowing the functions declared when it starts (see
/// [`compile_with_functions`]).
pub fn compile_program_with_functions(
    program: ast::Program,
    source: Rc<[u8]>,
    opts: CompileOptions,
    functions: &dyn FunctionTable,
) -> CResult<CompiledUnit> {
    compile_program_inner(program, source, opts, Some(functions))
}

fn compile_program_inner(
    mut program: ast::Program,
    source: Rc<[u8]>,
    opts: CompileOptions,
    functions: Option<&dyn FunctionTable>,
) -> CResult<CompiledUnit> {
    let mut c = Compiler::start(&program, source, opts, functions);
    let statements = std::mem::take(&mut program.statements);
    drop(program);
    c.compile_top_statements_owned(statements)?;
    c.finish_unit()
}

impl<'f> Compiler<'f> {
    /// A declaration's doc comment for the compiled code: none when the
    /// script is compiled for opcache under `opcache.save_comments=0`.
    pub(crate) fn doc_comment(&self, doc: Option<&Vec<u8>>) -> Option<ZStr> {
        if self.opts.strip_doc_comments {
            return None;
        }
        doc.map(|d| ZStr::from(d.as_slice()))
    }

    /// The compiler state for a new file (`zend_compile_top_stmt` callers):
    /// the main op array is open and the parser's diagnostics are queued.
    fn start(
        program: &ast::Program,
        source: Rc<[u8]>,
        opts: CompileOptions,
        functions: Option<&'f dyn FunctionTable>,
    ) -> Self {
        let mut c = Compiler {
            filename: opts.filename.clone(),
            namespace: Vec::new(),
            class_uses: HashMap::new(),
            fn_uses: HashMap::new(),
            const_uses: HashMap::new(),
            seen_symbols: HashMap::new(),
            strict_types: false,
            ticks: 0,
            decls: Rc::new(DeclTable::default()),
            empty_decls: Rc::new(DeclTable::default()),
            fns: Vec::new(),
            classes: Vec::new(),
            hoisted_functions: Vec::new(),
            hoisted_classes: Vec::new(),
            warnings: Vec::new(),
            anon_counter: opts.anon_base,
            stack: stack::StackModel::new(opts.stack_limit),
            opts,
            declared_functions: HashMap::new(),
            known_functions: HashMap::new(),
            functions,
            source,
            multibyte: program.multibyte,
            hook_prop: None,
            closure_depth: 0,
            in_const_default: false,
            in_const_decl: false,
            inline_const_depth: 0,
            in_global_const: false,
            attr_owner: None,
            no_const_subst: false,
        };
        for w in &program.warnings {
            let level = match w.level {
                ast::DiagnosticLevel::CompileWarning => 128,
                ast::DiagnosticLevel::Deprecated => 8192,
                ast::DiagnosticLevel::Warning => 2,
            };
            c.warnings.push((level, w.message.clone(), w.line));
        }
        c.strict_types = c.opts.strict_types;
        if let Some(cls) = c.opts.eval_class.clone() {
            c.classes.push(ClassCtx {
                name: cls,
                parent: c.opts.eval_parent.clone(),
                is_trait: false,
                is_interface: false,
                is_anon: false,
            });
        }
        let mut f = FnCtx::new(FnKind::Main);
        f.line = program.statements.first().map_or(1, |s| s.span.line);
        f.decls = Some(c.decls.clone());
        c.fns.push(f);
        c
    }

    /// Closes the main op array and returns the unit.
    fn finish_unit(mut self) -> CResult<CompiledUnit> {
        let c = &mut self;
        let line = c.f().line;
        let one = c.lit(Value::Int(1));
        c.f().line = line;
        c.emit(Op::Return { val: one });
        let f = c.fns.pop().expect("main fn ctx");
        let code = c.finish_fn(f)?;
        let main = Rc::new(UserFunction {
            name: ZStr::empty(),
            params: Vec::new(),
            required: 0,
            return_type: None,
            flags: 0,
            code: Rc::new(code),
            line_start: 1,
            line_end: 0,
            doc_comment: None,
            statics: std::cell::RefCell::new(Vec::new()),
            uses: Vec::new(),
            class_name: None,
            attributes: Vec::new(),
            hook_prop: None,
            call_shape: crate::func::CallShape::default(),
        });
        Ok(CompiledUnit {
            main,
            hoisted_functions: std::mem::take(&mut c.hoisted_functions),
            hoisted_classes: std::mem::take(&mut c.hoisted_classes),
            decls: c.decls.clone(),
            warnings: std::mem::take(&mut c.warnings),
            anon_end: c.anon_counter,
            meta: UnitMeta::default(),
        })
    }
}

impl Compiler<'_> {
    /// The current function context.
    pub(crate) fn f(&mut self) -> &mut FnCtx {
        self.fns.last_mut().expect("function context")
    }

    pub(crate) fn fref(&self) -> &FnCtx {
        self.fns.last().expect("function context")
    }

    pub(crate) fn err<T>(&self, msg: impl Into<String>, line: u32) -> CResult<T> {
        Err(CompileError::new(msg, line, self.warnings.clone()))
    }

    /// The `Op::UserOpcode` before an instruction implementing `opcode`,
    /// when that Zend opcode has a user handler.
    pub(crate) fn user_opcode_hook(&mut self, opcode: &'static str) {
        if self.opts.user_opcodes.contains(&opcode) {
            let name = self.lit_str(opcode.as_bytes());
            self.emit(Op::UserOpcode { opcode: name });
        }
    }

    pub(crate) fn emit(&mut self, op: Op) -> usize {
        let f = self.f();
        let line = f.line;
        f.ops.push(op);
        f.lines.push(line);
        f.ops.len() - 1
    }

    pub(crate) fn here(&mut self) -> Label {
        u32::try_from(self.f().ops.len()).unwrap_or(u32::MAX)
    }

    pub(crate) fn tmp(&mut self) -> u32 {
        let f = self.f();
        f.num_tmps += 1;
        f.num_tmps - 1
    }

    pub(crate) fn lit(&mut self, v: Value) -> Operand {
        Operand::Lit(self.lit_idx(v))
    }

    pub(crate) fn lit_idx(&mut self, v: Value) -> u32 {
        let f = self.f();
        let v = match v {
            Value::Str(s) => {
                if let Some(&i) = f.lit_strs.get(s.as_bytes()) {
                    return i;
                }
                // `zend_insert_literal` -> `zval_make_interned_string`: a
                // literal nothing else holds becomes the interned instance
                // itself, without a copy.
                let s = s.into_interned();
                let i = u32::try_from(f.literals.len()).unwrap_or(u32::MAX);
                f.lit_strs.insert(s.clone(), i);
                f.literals.push(Value::Str(s));
                return i;
            }
            Value::Array(mut a) => {
                intern_array_strings(&mut a);
                Value::Array(a)
            }
            other => other,
        };
        f.literals.push(v);
        u32::try_from(f.literals.len() - 1).unwrap_or(u32::MAX)
    }

    pub(crate) fn lit_str(&mut self, s: &[u8]) -> u32 {
        if let Some(&i) = self.f().lit_strs.get(s) {
            return i;
        }
        self.lit_idx(Value::Str(ZStr::from(s)))
    }

    pub(crate) fn cv(&mut self, name: &[u8]) -> u32 {
        let f = self.f();
        if let Some(&i) = f.cv_map.get(name) {
            return i;
        }
        let i = u32::try_from(f.cv_names.len()).unwrap_or(u32::MAX);
        f.cv_names.push(ZStr::from(name));
        f.cv_map.insert(name.to_vec(), i);
        i
    }

    /// `zend_try_compile_cv` with `BP_VAR_R` (`Zend/zend_compile.c`):
    /// reading the CV `$http_response_header` before this op array has
    /// assigned it raises the compile-time deprecation.
    pub(crate) fn note_cv_read(&mut self, name: &[u8], line: u32) {
        if name == b"http_response_header" {
            self.http_response_header_read(line);
        }
    }

    #[cold]
    fn http_response_header_read(&mut self, line: u32) {
        if !self.f().http_response_header_assigned {
            self.push_http_response_header_deprecation(line);
        }
    }

    /// `zend_try_compile_cv` with `BP_VAR_W` and `zend_compile_params`:
    /// writing `$http_response_header` silences later reads in this op
    /// array.
    pub(crate) fn note_cv_write(&mut self, name: &[u8]) {
        if name == b"http_response_header" {
            self.f().http_response_header_assigned = true;
        }
    }

    /// The read side of a compiled place: a CV base goes through
    /// [`Self::note_cv_read`], and a `${'http_response_header'}` base with
    /// a constant name always warns (`zend_compile_simple_var_no_cv`).
    pub(crate) fn note_place_read(&mut self, p: &Place, line: u32) {
        match p.base {
            PlaceBase::Cv(i) => {
                let is_hrh = self
                    .fref()
                    .cv_names
                    .get(i as usize)
                    .is_some_and(|n| n.as_bytes() == b"http_response_header");
                if is_hrh {
                    self.http_response_header_read(line);
                }
            }
            PlaceBase::VarVar(Operand::Lit(i)) => {
                let is_hrh = matches!(
                    self.fref().literals.get(i as usize),
                    Some(Value::Str(s)) if s.as_bytes() == b"http_response_header"
                );
                if is_hrh {
                    self.push_http_response_header_deprecation(line);
                }
            }
            _ => {}
        }
    }

    #[cold]
    fn push_http_response_header_deprecation(&mut self, line: u32) {
        self.warnings.push((
            8192,
            "The predefined locally scoped $http_response_header variable is deprecated, call http_get_last_response_headers() instead".to_string(),
            line,
        ));
    }

    #[allow(dead_code)]
    pub(crate) fn has_cv(&self, name: &[u8]) -> bool {
        self.fref().cv_map.contains_key(name)
    }

    pub(crate) fn place(&mut self, p: Place) -> PlaceId {
        let f = self.f();
        f.places.push(p);
        u32::try_from(f.places.len() - 1).unwrap_or(u32::MAX)
    }

    /// Patches the jump target of op `at` to `target`.
    pub(crate) fn patch(&mut self, at: usize, target: Label) {
        let op = &mut self.f().ops[at];
        match op {
            Op::Jmp(t)
            | Op::JmpZ(_, t)
            | Op::JmpNz(_, t)
            | Op::JmpZEx { target: t, .. }
            | Op::JmpNzEx { target: t, .. }
            | Op::JmpSet { target: t, .. }
            | Op::Coalesce { target: t, .. }
            | Op::JmpNull { target: t, .. }
            | Op::New { skip: t, .. }
            | Op::RecvInit { skip: t, .. }
            | Op::BindStatic { skip: t, .. }
            | Op::FastCall { target: t, .. }
            | Op::FeReset { end: t, .. }
            | Op::FeFetch { end: t, .. }
            | Op::Catch { next: t, .. }
            | Op::AssertCheck { skip: t, .. } => *t = target,
            _ => {}
        }
    }

    pub(crate) fn patch_here(&mut self, at: usize) {
        let h = self.here();
        self.patch(at, h);
    }

    /// Turns an operand into one that can be read twice: tmps become
    /// `TmpKeep`.
    pub(crate) fn keep(op: Operand) -> Operand {
        match op {
            Operand::Tmp(t) => Operand::TmpKeep(t),
            o => o,
        }
    }

    /// Finalises a function context into an op array.
    pub(crate) fn finish_fn(&mut self, mut f: FnCtx) -> CResult<OpArray> {
        let is_main = f.kind == FnKind::Main;
        // Resolve gotos.
        for PendingGoto {
            at,
            label,
            line,
            loops,
            unwind,
        } in std::mem::take(&mut f.gotos)
        {
            let Some((target, label_loops)) = f.labels.get(&label) else {
                return self.err(
                    format!(
                        "'goto' to undefined label '{}'",
                        String::from_utf8_lossy(&label)
                    ),
                    line,
                );
            };
            // `zend_check_finally_breakout`.
            let target_op = *target;
            let at_op = u32::try_from(at).unwrap_or(u32::MAX);
            for (e, _) in &f.try_table {
                let Some((_, fs, fe)) = e.finally else {
                    continue;
                };
                let last = fe.saturating_sub(1);
                let in_fin = |op: u32| op >= fs && op <= last;
                if !in_fin(at_op) && in_fin(target_op) {
                    return self.err("jump into a finally block is disallowed", line);
                }
                if in_fin(at_op) && !in_fin(target_op) {
                    return self.err("jump out of a finally block is disallowed", line);
                }
            }
            // A jump into a loop is not allowed.
            if !label_loops.iter().all(|l| loops.contains(l)) {
                return self.err("'goto' into loop or switch statement is disallowed", line);
            }
            // `zend_resolve_goto_label`: the loop frees and finally calls of
            // the constructs the jump stays inside become NOPs.
            for (op, u) in unwind {
                let crossed = match u {
                    GotoUnwind::Loop(id) => !label_loops.contains(&id),
                    GotoUnwind::Finally(fid) => f
                        .try_table
                        .iter()
                        .find_map(|(e, _)| match e.finally {
                            Some((id, _, fe)) if id == fid => {
                                Some(target_op < e.try_start || target_op >= fe)
                            }
                            _ => None,
                        })
                        .unwrap_or(true),
                };
                if !crossed {
                    f.ops[op] = Op::Nop;
                }
            }
            if let Op::Jmp(t) = &mut f.ops[at] {
                *t = target_op;
            }
        }
        let mut try_table: Vec<(TryEntry, Label)> = std::mem::take(&mut f.try_table);
        try_table.sort_by(|a, b| b.0.try_start.cmp(&a.0.try_start));
        let mut op_array = OpArray {
            ops: f.ops,
            lines: f.lines,
            literals: f.literals,
            places: f.places,
            cv_names: f.cv_names,
            num_tmps: f.num_tmps,
            num_iters: f.num_iters,
            num_finally: f.num_finally,
            num_statics: f.num_statics,
            static_vars: f.static_vars,
            try_table: try_table.into_iter().map(|(e, _)| e).collect(),
            live_tmps: f.live_tmps,
            filename: self.filename.clone(),
            strict_types: self.strict_types,
            decls: f.decls.unwrap_or_else(|| self.empty_decls.clone()),
            fcall_cache: std::cell::RefCell::default(),
            class_cache: std::cell::RefCell::default(),
            rt_cache: crate::rtcache::RuntimeCache::default(),
            prop_cache: std::cell::RefCell::default(),
            method_cache: std::cell::RefCell::default(),
            tape: crate::exec::tape::Tape::default(),
        };
        // Optimizer passes run when opcache.optimization_level is nonzero
        // and no user opcode handlers are hooked.
        let mut reduced_to_literal = false;
        if self.opts.optimization_level != 0 && self.opts.user_opcodes.is_empty() {
            let raw_literal = optimizer::dump::returns_literal(&op_array);
            // Top-level code's CVs are the global symbol table: other
            // functions reach them through `global`/`$GLOBALS` and included
            // files share them, so no assignment there is ever dead (php-src
            // does not remove unused variables of pseudo-main code).
            let level = if is_main {
                self.opts.optimization_level & !optimizer::PASS_13
            } else {
                self.opts.optimization_level
            };
            let ctx = optimizer::OptContext {
                is_main,
                bound_cvs: &f.bound_cvs,
                scalar_params: &f.scalar_params,
                sapi_name: self
                    .opts
                    .ct_constants
                    .as_ref()
                    .and_then(|c| c.get(b"PHP_SAPI")),
                verifies_return: f.has_return_type && !f.is_generator && !f.returns_ref,
            };
            optimizer::optimize_op_array_with(&mut op_array, level, &ctx);
            refresh_static_inits(&mut op_array);
            reduced_to_literal = !raw_literal && optimizer::dump::returns_literal(&op_array);
        }
        // Handler specialisation, last: every jump target is final. A
        // `zend_set_user_opcode_handler()` hook runs before its generic
        // opcode, so hooked code keeps the generic ops.
        if self.opts.user_opcodes.is_empty() {
            specialise::specialise(&mut op_array.ops, &op_array.places, &op_array.literals);
            specialise::fuse_frameless_calls(&mut op_array);
        }
        // The opcache dump inlines calls of a function only if its compiled
        // code (not the optimized one) is a literal return.
        if reduced_to_literal {
            optimizer::dump::note_reduced_to_literal(&op_array);
        }
        Ok(op_array)
    }

    // ------------------------------------------------------------------
    // Name resolution
    // ------------------------------------------------------------------

    fn ns_prefixed(&self, name: &[u8]) -> Vec<u8> {
        if self.namespace.is_empty() {
            name.to_vec()
        } else {
            let mut v = self.namespace.clone();
            v.push(b'\\');
            v.extend_from_slice(name);
            v
        }
    }

    /// `zend_resolve_class_name`.
    pub(crate) fn resolve_class_name(&self, name: &Name) -> Vec<u8> {
        match name.kind {
            NameKind::FullyQualified => name.name.clone(),
            NameKind::Relative => self.ns_prefixed(&name.name),
            NameKind::Unqualified | NameKind::Qualified => {
                let (first, rest) = match name.name.iter().position(|&c| c == b'\\') {
                    Some(p) => (&name.name[..p], Some(&name.name[p..])),
                    None => (&name.name[..], None),
                };
                if let Some(imp) = self.class_uses.get(&first.to_ascii_lowercase()) {
                    let mut v = imp.clone();
                    if let Some(r) = rest {
                        v.extend_from_slice(r);
                    }
                    return v;
                }
                self.ns_prefixed(&name.name)
            }
        }
    }

    /// Resolves a function name: returns (name, fallback global name).
    pub(crate) fn resolve_function_name(&self, name: &Name) -> (Vec<u8>, Option<Vec<u8>>) {
        match name.kind {
            NameKind::FullyQualified => (name.name.clone(), None),
            NameKind::Relative => (self.ns_prefixed(&name.name), None),
            NameKind::Qualified => (self.resolve_class_name(name), None),
            NameKind::Unqualified => {
                if let Some(imp) = self.fn_uses.get(&name.name.to_ascii_lowercase()) {
                    return (imp.clone(), None);
                }
                if self.namespace.is_empty() {
                    (name.name.clone(), None)
                } else {
                    (self.ns_prefixed(&name.name), Some(name.name.clone()))
                }
            }
        }
    }

    /// Resolves a constant name: returns (name, fallback).
    pub(crate) fn resolve_const_name(&self, name: &Name) -> (Vec<u8>, Option<Vec<u8>>) {
        match name.kind {
            NameKind::FullyQualified => (name.name.clone(), None),
            NameKind::Relative => (self.ns_prefixed(&name.name), None),
            NameKind::Qualified => (self.resolve_class_name(name), None),
            NameKind::Unqualified => {
                if let Some(imp) = self.const_uses.get(&name.name) {
                    return (imp.clone(), None);
                }
                if self.namespace.is_empty() {
                    (name.name.clone(), None)
                } else {
                    (self.ns_prefixed(&name.name), Some(name.name.clone()))
                }
            }
        }
    }

    /// Builds a [`ClassSpec`] from a class name used in an expression.
    pub(crate) fn class_spec_name(&mut self, name: &Name) -> CResult<ClassSpec> {
        if name.kind == NameKind::Unqualified {
            let lc = name.name.to_ascii_lowercase();
            match lc.as_slice() {
                b"self" => {
                    self.check_class_scope("self", name.span.line)?;
                    return Ok(ClassSpec::SelfClass);
                }
                b"parent" => {
                    self.check_class_scope("parent", name.span.line)?;
                    return Ok(ClassSpec::Parent);
                }
                b"static" => {
                    self.check_class_scope("static", name.span.line)?;
                    return Ok(ClassSpec::Static);
                }
                _ => {}
            }
        }
        let resolved = self.resolve_class_name(name);
        let l = self.lit_str(&resolved);
        Ok(ClassSpec::Named(l))
    }

    /// The compile-time `self`/`parent`/`static` check of a class
    /// reference. Constant expressions (parameter defaults, constant and
    /// property initializers) defer it to evaluation: the
    /// `zend_compile_const_expr_*` helpers for `new` and `X::C` do not call
    /// `zend_ensure_valid_class_fetch_type()`.
    fn check_class_scope(&self, what: &str, line: u32) -> CResult<()> {
        if self.in_const_expr() {
            return Ok(());
        }
        self.ensure_valid_class_fetch(what, line)
    }

    /// Whether a constant expression is being compiled.
    pub(crate) fn in_const_expr(&self) -> bool {
        self.in_const_default || self.fref().kind == FnKind::ConstExpr
    }

    /// Whether the code being compiled is a constant expression evaluated
    /// by `zend_ast_evaluate_ex` (not a closure body inside one).
    pub(crate) fn in_evaluated_const_expr(&self) -> bool {
        self.fref().kind == FnKind::ConstExpr || self.inline_const_depth == self.fns.len()
    }

    /// `zend_ensure_valid_class_fetch_type()`: `self`/`parent`/`static`
    /// are rejected at compile time only where the scope is known
    /// (`zend_is_scope_known()`): not in closures, nor in file or eval
    /// code (which inherits the including scope), nor in traits; elsewhere
    /// the runtime fetch reports the error.
    pub(crate) fn ensure_valid_class_fetch(&self, what: &str, line: u32) -> CResult<()> {
        let in_closure = self
            .fns
            .iter()
            .rev()
            .take_while(|f| f.kind != FnKind::Main)
            .any(|f| f.kind == FnKind::Closure);
        if in_closure {
            return Ok(());
        }
        let Some(c) = self.classes.last() else {
            let kind = self
                .fns
                .iter()
                .rev()
                .map(|f| f.kind)
                .find(|k| *k != FnKind::ConstExpr)
                .unwrap_or(FnKind::Main);
            if kind == FnKind::Main {
                return Ok(());
            }
            return self.err(
                format!("Cannot use \"{what}\" when no class scope is active"),
                line,
            );
        };
        if what == "parent" && c.parent.is_none() && !c.is_trait {
            return self.err(
                "Cannot use \"parent\" when current class scope has no parent",
                line,
            );
        }
        Ok(())
    }

    pub(crate) fn class_spec(&mut self, cref: &ast::ClassRef) -> CResult<ClassSpec> {
        match cref {
            ast::ClassRef::Name(n) => self.class_spec_name(n),
            ast::ClassRef::Expr(e) => {
                // `zend_compile_class_ref`: a class reference that compiles
                // to a constant other than a string is "Illegal class name".
                if !matches!(self.try_fold(e), None | Some(Value::Str(_))) {
                    return self.err("Illegal class name", e.span.line);
                }
                let op = self.compile_expr(e)?;
                Ok(ClassSpec::Dyn(op))
            }
        }
    }

    /// The current class name (for `__CLASS__`, `self` in types).
    pub(crate) fn current_class(&self) -> Option<&ClassCtx> {
        self.classes.last()
    }

    pub(crate) fn attributes(&mut self, groups: &ast::Attributes) -> CResult<Vec<AttributeInfo>> {
        self.attributes_at(groups, None)
    }

    /// [`Compiler::attributes`] of a declaration starting on `decl_line`:
    /// `zend_compile_attributes()` reports its own argument-list errors on
    /// `CG(zend_lineno)`, the declaration's line rather than the attribute's.
    pub(crate) fn attributes_at(
        &mut self,
        groups: &ast::Attributes,
        decl_line: Option<u32>,
    ) -> CResult<Vec<AttributeInfo>> {
        let mut out = Vec::new();
        for g in groups {
            for a in &g.attributes {
                let name = ZStr::from(self.resolve_class_name(&a.name));
                let mut args: Vec<(Option<ZStr>, ConstExpr)> = Vec::new();
                if let Some(al) = &a.args {
                    if al.is_first_class_callable() {
                        return self.err(
                            "Cannot create Closure as attribute argument",
                            decl_line.unwrap_or(a.span.line),
                        );
                    }
                    let mut uses_named = false;
                    for arg in &al.args {
                        match &arg.kind {
                            ast::ArgKind::Positional(e) => {
                                if uses_named {
                                    return self.err(
                                        "Cannot use positional argument after named argument",
                                        decl_line.unwrap_or(arg.span.line),
                                    );
                                }
                                self.check_const_expr(e, true)?;
                                args.push((None, self.const_expr(e)?));
                            }
                            ast::ArgKind::Named { name, value } => {
                                uses_named = true;
                                let n = ZStr::from(name.name.as_slice());
                                if args.iter().any(|(k, _)| k.as_ref() == Some(&n)) {
                                    return self.err(
                                        format!(
                                            "Duplicate named parameter ${}",
                                            n.to_string_lossy()
                                        ),
                                        decl_line.unwrap_or(arg.span.line),
                                    );
                                }
                                self.check_const_expr(value, true)?;
                                args.push((Some(n), self.const_expr(value)?));
                            }
                            ast::ArgKind::Unpack(_) => {
                                return self.err(
                                    "Cannot use unpacking in attribute argument list",
                                    decl_line.unwrap_or(arg.span.line),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                out.push(AttributeInfo {
                    name,
                    args,
                    line: a.span.line,
                    strict: self.strict_types,
                });
            }
        }
        Ok(out)
    }

    /// Compiles a constant expression into a [`ConstExpr`].
    pub(crate) fn const_expr(&mut self, e: &ast::Expr) -> CResult<ConstExpr> {
        if let Some(v) = self.try_fold(e) {
            let v = match v {
                Value::Str(s) => Value::Str(s.into_interned()),
                Value::Array(mut a) => {
                    intern_array_strings(&mut a);
                    Value::Array(a)
                }
                other => other,
            };
            return Ok(ConstExpr::Value(v));
        }
        let mut f = FnCtx::new(FnKind::ConstExpr);
        f.line = e.span.line;
        self.fns.push(f);
        let r = self.compile_expr(e);
        let r = match r {
            Ok(op) => {
                self.emit(Op::Return { val: op });
                Ok(())
            }
            Err(err) => Err(err),
        };
        let f = self.fns.pop().expect("const expr ctx");
        r?;
        let code = self.finish_fn(f)?;
        Ok(ConstExpr::Code(Rc::new(code)))
    }

    /// The declaration table of the op array being compiled, created on
    /// its first declaration.
    pub(crate) fn current_decls(&mut self) -> Rc<DeclTable> {
        self.f()
            .decls
            .get_or_insert_with(|| Rc::new(DeclTable::default()))
            .clone()
    }

    /// Whether `table` is a nested op array's, whose declarations are also
    /// listed in the unit's table ([`Compiler::decls`]).
    fn is_nested_table(&self, table: &Rc<DeclTable>) -> bool {
        !Rc::ptr_eq(table, &self.decls)
    }

    /// Registers a class declaration of the current op array; returns its
    /// index in that op array's table.
    pub(crate) fn add_class(&mut self, decl: ClassDecl) -> u32 {
        let table = self.current_decls();
        let decl = Rc::new(decl);
        if self.is_nested_table(&table) {
            self.decls.classes.borrow_mut().push(decl.clone());
        }
        let mut cs = table.classes.borrow_mut();
        cs.push(decl);
        u32::try_from(cs.len() - 1).unwrap_or(u32::MAX)
    }

    /// Registers a function or closure declared in the current op array;
    /// returns its index in that op array's table.
    pub(crate) fn add_function(&mut self, f: UserFunction) -> u32 {
        let table = self.current_decls();
        let f = Rc::new(f);
        if self.is_nested_table(&table) {
            self.decls.functions.borrow_mut().push(f.clone());
        }
        let mut fs = table.functions.borrow_mut();
        fs.push(f);
        u32::try_from(fs.len() - 1).unwrap_or(u32::MAX)
    }
}

/// Whether a folded array holds an empty or one-byte string that is not
/// the shared interned instance.
fn has_uninterned_strings(a: &php_runtime::ZArray) -> bool {
    a.values().any(|v| match v {
        Value::Str(s) => s.len() <= 1 && !s.is_interned(),
        Value::Array(inner) => has_uninterned_strings(inner),
        _ => false,
    })
}

/// The scanner makes empty and one-byte string literals the shared
/// interned `ZSTR_EMPTY_ALLOC()` / `ZSTR_CHAR(c)` (`zend_scan_escape_string`);
/// a compile-time array keeps those zvals (`zend_try_ct_eval_array`), so
/// `debug_zval_dump()` shows them "interned" (longer strings are not).
fn intern_array_strings(a: &mut php_runtime::ZArray) {
    if !has_uninterned_strings(a) {
        return;
    }
    for (_, v) in a.make_mut().iter_mut() {
        match v {
            Value::Str(s) if s.len() <= 1 && !s.is_interned() => *s = s.interned(),
            Value::Array(inner) => intern_array_strings(inner),
            _ => {}
        }
    }
}

/// Records in [`OpArray::static_vars`] the constant initializers that the
/// optimizer folded into a literal `InitStatic` operand, so reflection shows
/// the value the declaration will assign.
fn refresh_static_inits(op_array: &mut OpArray) {
    for op in &op_array.ops {
        if let Op::InitStatic {
            idx,
            val: Operand::Lit(l),
            ..
        } = op
        {
            if let (Some(d), Some(v)) = (
                op_array.static_vars.get_mut(*idx as usize),
                op_array.literals.get(*l as usize),
            ) {
                d.init = Some(v.clone());
            }
        }
    }
}
