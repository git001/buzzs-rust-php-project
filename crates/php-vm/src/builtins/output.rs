// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Output functions: `printf` family and output buffering (`ob_*`).

use super::reg;
use super::util::{arr, b, i, s, sv};
use crate::func::{flags, Args, Func};
use crate::object;
use crate::vm::{PendingCall, Vm, VmResult};
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_support::fmt::{php_formatted_print, ArgSource, FormatArg, FormatError, FormatOptions};
use php_support::output::{self as ob, StartHandler};
use std::borrow::Cow;

const E_WARNING: i64 = 2;

/// A pre-converted `sprintf` argument.
struct FArg {
    int: i64,
    float: f64,
    string: Vec<u8>,
    exact: Option<i64>,
}

impl FormatArg for FArg {
    fn as_int(&self) -> i64 {
        self.int
    }
    fn as_float(&self) -> f64 {
        self.float
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.string)
    }
    fn exact_int(&self) -> Option<i64> {
        self.exact
    }
}

fn convert_args(vm: &mut Vm, vals: &[Value]) -> VmResult<Vec<FArg>> {
    let mut out = Vec::with_capacity(vals.len());
    for v in vals {
        let v = v.deref();
        let (int, float) = match &v {
            Value::Int(n) => {
                #[allow(clippy::cast_precision_loss)]
                let f = *n as f64;
                (*n, f)
            }
            Value::Float(f) => (php_runtime::convert::dval_to_lval_cap(*f).min(i64::MAX), *f),
            Value::Bool(b) => (i64::from(*b), f64::from(u8::from(*b))),
            Value::Null => (0, 0.0),
            Value::Str(s) => {
                let n = php_runtime::numeric::is_numeric_string_ex(s.as_bytes(), true);
                match n.map(|x| x.value) {
                    Some(php_runtime::numeric::Number::Int(i)) => {
                        #[allow(clippy::cast_precision_loss)]
                        let f = i as f64;
                        (i, f)
                    }
                    Some(php_runtime::numeric::Number::Float(f)) => {
                        (php_runtime::convert::dval_to_lval_cap(f), f)
                    }
                    None => (0, 0.0),
                }
            }
            Value::Array(a) => (i64::from(!a.is_empty()), f64::from(u8::from(!a.is_empty()))),
            _ => (1, 1.0),
        };
        let int = match &v {
            Value::Float(f) => {
                if f.is_finite() {
                    php_runtime::convert::dval_to_lval_modular(*f)
                } else {
                    0
                }
            }
            _ => int,
        };
        let string = match &v {
            Value::Str(s) => s.as_bytes().to_vec(),
            other => vm.to_zstr(other)?.as_bytes().to_vec(),
        };
        out.push(FArg {
            int,
            float,
            string,
            exact: if let Value::Int(n) = v { Some(n) } else { None },
        });
    }
    Ok(out)
}

fn do_format(vm: &mut Vm, fmt: &[u8], vals: &[Value], source: ArgSource) -> VmResult<Vec<u8>> {
    let args = convert_args(vm, vals)?;
    let refs: Vec<&dyn FormatArg> = args.iter().map(|a| a as &dyn FormatArg).collect();
    let mut diags: Vec<php_support::Diagnostic> = Vec::new();
    let opts = FormatOptions {
        source,
        decimal_point: php_support::fmt::locale::decimal_point(),
    };
    let r = php_formatted_print(fmt, &refs, opts, &mut diags);
    for d in diags {
        let lvl = i64::from(d.level.0);
        if d.docref {
            vm.docref(lvl, &d.message)?;
        } else {
            vm.error(lvl, &d.message)?;
        }
    }
    match r {
        Ok(v) => Ok(v),
        Err(FormatError::ValueError(m)) => Err(vm.make_throw("ValueError", &m)),
        Err(FormatError::ArgumentCountError(m)) => Err(vm.make_throw("ArgumentCountError", &m)),
        Err(FormatError::Fatal(m)) => Err(vm.fatal(&m)),
        Err(FormatError::MemoryLimit(over)) => Err(over.into()),
    }
}

fn sprintf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = s(a, 0);
    let vals: Vec<Value> = a.values[1..].to_vec();
    Ok(sv(do_format(
        vm,
        f.as_bytes(),
        &vals,
        ArgSource::Variadic(1),
    )?))
}

fn printf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = s(a, 0);
    let vals: Vec<Value> = a.values[1..].to_vec();
    let out = do_format(vm, f.as_bytes(), &vals, ArgSource::Variadic(1))?;
    vm.echo(&out);
    Ok(Value::Int(i64::try_from(out.len()).unwrap_or(0)))
}

fn vsprintf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = s(a, 0);
    let vals: Vec<Value> = arr(a, 1).iter().map(|(_, v)| v.clone()).collect();
    Ok(sv(do_format(vm, f.as_bytes(), &vals, ArgSource::Array)?))
}

fn vprintf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = s(a, 0);
    let vals: Vec<Value> = arr(a, 1).iter().map(|(_, v)| v.clone()).collect();
    let out = do_format(vm, f.as_bytes(), &vals, ArgSource::Array)?;
    vm.echo(&out);
    Ok(Value::Int(i64::try_from(out.len()).unwrap_or(0)))
}

fn handler_name(vm: &mut Vm, cb: &Value) -> Vec<u8> {
    vm.callable_name(cb).into_bytes()
}

fn ob_start(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    let chunk = i(a, 1);
    let fl = if a.has(2) { i(a, 2) } else { 112 };
    let alias = match &cb {
        Value::Str(n) => vm
            .output_handler_aliases
            .get(n)
            .copied()
            .map(|alias| (n.as_bytes().to_vec(), alias)),
        _ => None,
    };
    let handler = if cb.is_null() {
        StartHandler::Default
    } else if let Some((name, alias)) = alias {
        if let Some(init) = alias.init {
            let size = usize::try_from(chunk.max(0)).unwrap_or(usize::MAX);
            #[allow(clippy::cast_possible_truncation)]
            let flags = fl as i32;
            init(vm, size, flags)?;
        }
        StartHandler::Alias { name, callback: cb }
    } else {
        // `php_output_handler_create_user()`: `zend_fcall_info_init()`
        // resolves the handler here, raising the callable deprecations, and
        // the handler calls that resolution whenever the buffer is flushed.
        match vm.resolve_kept_callable(&cb)? {
            Ok(callback) => {
                let name = handler_name(vm, &cb);
                StartHandler::User { name, callback }
            }
            Err(err) => {
                if vm.output.status() & ob::status::LOCKED != 0 {
                    // `php_output_handler_create_user()` warns, then
                    // `php_output_handler_start()` hits the lock error (fatal).
                    vm.docref(E_WARNING, &err)?;
                    StartHandler::Default
                } else {
                    StartHandler::Invalid(err)
                }
            }
        }
    };
    let r = ob::ob_start(vm, handler, chunk, fl);
    vm.take_pending_throw()?;
    Ok(Value::Bool(r))
}

fn ob_get_clean(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let r = ob::ob_get_clean(vm);
    vm.take_pending_throw()?;
    Ok(r.map_or(Value::Bool(false), sv))
}

fn ob_get_flush(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let r = ob::ob_get_flush(vm);
    vm.take_pending_throw()?;
    Ok(r.map_or(Value::Bool(false), sv))
}

fn ob_get_contents(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(ob::ob_get_contents(vm).map_or(Value::Bool(false), sv))
}

fn ob_get_length(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(ob::ob_get_length(vm).map_or(Value::Bool(false), |n| {
        Value::Int(i64::try_from(n).unwrap_or(0))
    }))
}

fn ob_get_level(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(i64::try_from(ob::ob_get_level(vm)).unwrap_or(0)))
}

fn ob_end_clean(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let r = ob::ob_end_clean(vm);
    vm.take_pending_throw()?;
    Ok(Value::Bool(r))
}

fn ob_end_flush(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let r = ob::ob_end_flush(vm);
    vm.take_pending_throw()?;
    Ok(Value::Bool(r))
}

fn ob_clean(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let r = ob::ob_clean(vm);
    vm.take_pending_throw()?;
    Ok(Value::Bool(r))
}

fn ob_flush(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let r = ob::ob_flush(vm);
    vm.take_pending_throw()?;
    Ok(Value::Bool(r))
}

fn ob_implicit_flush(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let on = !a.has(0) || b(a, 0);
    ob::ob_implicit_flush(vm, on);
    Ok(Value::Null)
}

fn ob_list_handlers(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let l = ob::ob_list_handlers(vm);
    Ok(Value::Array(ZArray::from_values(l.into_iter().map(sv))))
}

fn status_array(st: &php_support::output::HandlerStatus) -> Value {
    let mut t = HashTable::new();
    let k = |s: &str| ArrayKey::Str(ZStr::from(s));
    t.insert(k("name"), sv(st.name.clone()));
    t.insert(k("type"), Value::Int(i64::from(st.kind)));
    t.insert(k("flags"), Value::Int(i64::from(st.flags)));
    t.insert(k("level"), Value::Int(i64::from(st.level)));
    t.insert(
        k("chunk_size"),
        Value::Int(i64::try_from(st.chunk_size).unwrap_or(0)),
    );
    t.insert(
        k("buffer_size"),
        Value::Int(i64::try_from(st.buffer_size).unwrap_or(0)),
    );
    t.insert(
        k("buffer_used"),
        Value::Int(i64::try_from(st.buffer_used).unwrap_or(0)),
    );
    Value::Array(ZArray::from_table(t))
}

fn ob_get_status(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if b(a, 0) {
        let l = vm.output.full_status();
        return Ok(Value::Array(ZArray::from_values(
            l.iter().map(status_array),
        )));
    }
    Ok(match vm.output.active_status() {
        Some(st) => status_array(&st),
        None => Value::Array(ZArray::new()),
    })
}

fn output_add_rewrite_var(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(true))
}

impl Vm {
    /// Rethrows an exception raised inside an output handler.
    pub(crate) fn take_pending_throw(&mut self) -> VmResult<()> {
        match self.pending_throw.take() {
            Some(t) => Err(t),
            None => Ok(()),
        }
    }

    /// `Z_PARAM_FUNC` / `zend_fcall_info_init()` for a callable that is
    /// kept and called later, possibly from another scope (`ob_start()`'s
    /// handler, a `CallbackFilterIterator`'s callback): it is resolved once,
    /// in the calling user frame's scope, raising the callable deprecations
    /// there, and php-src calls that `zend_fcall_info_cache` afterwards.
    ///
    /// Returns the value to keep and call: `v` itself when calling it again
    /// resolves the same way from any scope (a function name, a closure or
    /// invokable object, a public method of an object or of a class named
    /// outright), else a fake closure of the resolution, so that later
    /// calls neither depend on their scope nor raise the deprecations again
    /// (`'parent::m'`, `[$this, 'privateMethod']`, `[$obj, 'C::m']`, a
    /// `$this` or called class taken from the scope). Unlike the
    /// `zend_fcall_info_cache`, the fake closure takes an object handle;
    /// only those scope-bound forms pay it. `Ok(Err(reason))` when `v` is
    /// not callable here.
    ///
    /// # Errors
    /// An exception thrown while resolving (an autoloader, a user error
    /// handler turning a deprecation into an exception).
    pub fn resolve_kept_callable(&mut self, v: &Value) -> VmResult<Result<Value, String>> {
        let call = match self.resolve_callable(v)? {
            Ok(call) => call,
            Err(reason) => return Ok(Err(reason)),
        };
        Ok(Ok(if scope_bound(v, &call) {
            self.fake_closure(call)
        } else {
            v.deref()
        }))
    }
}

/// Whether calling `v` from another scope could resolve differently from
/// `call`, its resolution in the calling scope: the `self`/`parent`/
/// `static` keywords and `[$obj, "C::m"]` names, a `$this` or called class
/// that `"C::m"` / `["C", "m"]` took from the scope, and methods only some
/// scopes may call.
fn scope_bound(v: &Value, call: &PendingCall) -> bool {
    let (class, method) = match v.deref() {
        Value::Str(s) => {
            let b = s.as_bytes();
            match b.windows(2).rposition(|w| w == b"::") {
                Some(p) => (Some(ZStr::from(&b[..p])), ZStr::from(&b[p + 2..])),
                // A function.
                None => return false,
            }
        }
        Value::Array(a) => {
            let Some(Value::Str(method)) = a.get(&ArrayKey::Int(1)).map(Value::deref) else {
                return true;
            };
            if method.as_bytes().windows(2).any(|w| w == b"::") {
                return true;
            }
            match a.get(&ArrayKey::Int(0)).map(Value::deref) {
                Some(Value::Str(c)) => (Some(c), method),
                Some(Value::Object(_)) => (None, method),
                _ => return true,
            }
        }
        // A closure or an object with `__invoke`.
        _ => return false,
    };
    if let Some(class) = class {
        let name = class.as_bytes();
        let name = name.strip_prefix(b"\\").unwrap_or(name);
        if matches!(
            name.to_ascii_lowercase().as_slice(),
            b"self" | b"parent" | b"static"
        ) {
            return true;
        }
        let named = call
            .called
            .as_ref()
            .is_some_and(|c| c.name.eq_ignore_ascii_case(name));
        if call.this.is_some() || !named {
            return true;
        }
    }
    restricted_method(call, &method)
}

/// Whether `call` is a method only some scopes may call: private or
/// protected, or a `__call`/`__callStatic` trampoline standing in for a
/// method `name` the resolving scope could not call.
fn restricted_method(call: &PendingCall, name: &ZStr) -> bool {
    if call.magic_name.is_some() {
        let class = match &call.this {
            Some(o) => Some(object::class_of(o).clone()),
            None => call.called.clone(),
        };
        return class.is_some_and(|c| c.find_method(name.as_bytes()).is_some());
    }
    // `scope` is the declaring class of a method (none for a function).
    call.scope.as_ref().is_some_and(|scope| {
        scope
            .methods
            .values()
            .any(|m| Func::ptr_eq(&m.func, &call.func) && m.visibility() != flags::PUBLIC)
    })
}

/// Registers the output-buffering functions.
pub(crate) fn register(vm: &mut Vm) {
    reg(vm, "ob_start($callback = null, int $chunk_size = 0, int $flags = PHP_OUTPUT_HANDLER_STDFLAGS): bool", ob_start);
    reg(vm, "ob_get_clean(): string|false", ob_get_clean);
    reg(vm, "ob_get_flush(): string|false", ob_get_flush);
    reg(vm, "ob_get_contents(): string|false", ob_get_contents);
    reg(vm, "ob_get_length(): int|false", ob_get_length);
    reg(vm, "ob_get_level(): int", ob_get_level);
    reg(vm, "ob_end_clean(): bool", ob_end_clean);
    reg(vm, "ob_end_flush(): bool", ob_end_flush);
    reg(vm, "ob_clean(): bool", ob_clean);
    reg(vm, "ob_flush(): bool", ob_flush);
    reg(
        vm,
        "ob_implicit_flush(bool $enable = true): void",
        ob_implicit_flush,
    );
    reg(vm, "ob_list_handlers(): array", ob_list_handlers);
    reg(
        vm,
        "ob_get_status(bool $full_status = false): array",
        ob_get_status,
    );
}

/// Registers the bare VM's formatted output and URL-rewriter functions
/// (`Vm::new`): the SAPIs get them from php-ext-stdstr and php-ext-session.
pub(crate) fn register_bare(vm: &mut Vm) {
    reg(
        vm,
        "sprintf(string $format, mixed ...$values): string",
        sprintf,
    );
    reg(vm, "printf(string $format, mixed ...$values): int", printf);
    reg(
        vm,
        "vsprintf(string $format, array $values): string",
        vsprintf,
    );
    reg(vm, "vprintf(string $format, array $values): int", vprintf);
    reg(
        vm,
        "output_add_rewrite_var(string $name, string $value): bool",
        output_add_rewrite_var,
    );
    reg(
        vm,
        "output_reset_rewrite_vars(): bool",
        output_add_rewrite_var,
    );
}
