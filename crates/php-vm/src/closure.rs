// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Closures (`Zend/zend_closures.c`): creation, binding, calling and the
//! `Closure` class; static variables.

use crate::builtins::classbuilder::ClassBuilder;
use crate::class::{cflags, Class, InternalKind};
use crate::func::{flags, Args, Builtin, Func, UserFunction};
use crate::object;
use crate::opcode::{Op, Operand, ParamInfo, PlaceBase};
use crate::vm::{FrameKind, PendingCall, Vm, VmResult};
use php_runtime::{ArrayKey, HashTable, Obj, PhpRef, Value, ZArray, ZStr};
use std::cell::RefCell;
use std::rc::Rc;

/// Internal state of a `Closure` object.
#[derive(Debug, Clone)]
pub struct ClosureData {
    /// Captured `use` variables: (CV index, value; references for by-ref).
    pub vars: Vec<(u32, Value)>,
    /// Static variables of this closure object.
    pub statics: Rc<RefCell<Vec<Option<PhpRef>>>>,
    /// Bound `$this`.
    pub this: Option<Obj>,
    /// Class scope.
    pub scope: Option<Rc<Class>>,
    /// Called scope.
    pub called: Option<Rc<Class>>,
    /// The function.
    pub func: Func,
    /// Created by `Closure::fromCallable` / first-class callable syntax.
    pub fake: bool,
    /// `__call` target name for fake closures of magic methods.
    pub magic_name: Option<ZStr>,
    /// Captured target closure of a partial application, rebound with its wrapper.
    pub partial_inner: Option<u32>,
}

/// A closure's static-variable store.
type StaticsStore = Rc<RefCell<Vec<Option<PhpRef>>>>;

thread_local! {
    /// The static-variable store of every closure that never writes one: a
    /// real closure whose function declares no static variables, and any
    /// fake closure (`Closure::fromCallable`, `f(...)`), which keeps using
    /// its function's. Only `ZEND_BIND_STATIC` writes a closure's store,
    /// through the frame ([`Vm::bind_closure_vars`] attaches it only when
    /// the function has static variables), so this one stays empty, and
    /// creating such a closure allocates no store of its own (php-src's
    /// `zend_create_closure` has none without `static_variables`).
    static NO_STATICS: StaticsStore = Rc::new(RefCell::new(Vec::new()));
}

/// Whether `func` is user code declaring static variables (`static $x`).
#[inline]
fn has_statics(func: &Func) -> bool {
    matches!(func, Func::User(u) if u.code.num_statics != 0)
}

/// The static-variable store of a new closure of `func`: `slots` in a
/// store of its own when it is a real closure of a function with static
/// variables, else the shared empty one ([`NO_STATICS`]).
fn closure_statics(func: &Func, fake: bool, slots: Vec<Option<PhpRef>>) -> StaticsStore {
    if !fake && has_statics(func) {
        Rc::new(RefCell::new(slots))
    } else {
        NO_STATICS.with(Rc::clone)
    }
}

/// Allocates the shared empty static-variable store at startup (the
/// `Closure` class registration), so that it is not counted against the
/// first request's memory usage when its first closure is created.
fn init_no_statics() {
    NO_STATICS.with(|_| ());
}

impl Vm {
    /// The `Closure` class (`zend_ce_closure`), looked up by its
    /// lowercase name without building a key.
    fn closure_class(&self) -> Option<Rc<Class>> {
        self.classes.get(&b"closure"[..]).cloned()
    }

    fn new_closure_object(&mut self, data: ClosureData) -> Obj {
        let cls = self.closure_class().expect("Closure class");
        let o = object::new_object(cls, Vec::new());
        object::set_internal(&o, data);
        o
    }

    /// Creates a closure for a declared function in the current frame.
    pub(crate) fn make_closure(&mut self, f: &Rc<UserFunction>) -> VmResult<Value> {
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: variables of the closure's use list"
        )]
        let mut vars = Vec::with_capacity(f.uses.len());
        for u in &f.uses {
            if u.by_ref {
                let fr = self.frame_mut();
                let idx = u.parent_cv as usize;
                let r = match fr.cvs.get_mut(idx).and_then(|c| c.as_mut().as_mut()) {
                    Some(Value::Reference(r)) => r.clone(),
                    Some(v) => v.make_ref(),
                    None => {
                        let r = PhpRef::new(Value::Null);
                        fr.cvs[idx].set(Some(Value::Reference(r.clone())));
                        r
                    }
                };
                // The parent CV now points at a reference (created by
                // `make_ref` or the `None` arm above). If the frame has an
                // attached symbol table (globals/include frames) that store
                // still holds the pre-reference value; keep it in sync so a
                // later `sync_from_symbol_table` does not clobber the shared
                // reference back to the stale value.
                let cv_value = Value::Reference(r.clone());
                fr.sync_cv_to_symtab(u.parent_cv, &cv_value);
                vars.push((u.cv, cv_value));
            } else if u.implicit {
                if let Some(v) = self.cv_get(u.parent_cv) {
                    vars.push((u.cv, v.deref()));
                }
            } else {
                let v = self.read_cv(u.parent_cv)?;
                vars.push((u.cv, v));
            }
        }
        let fr = self.frame();
        let is_static = f.flags & flags::STATIC != 0;
        let this = if is_static { None } else { fr.this.clone() };
        let called = match &fr.this {
            Some(t) => Some(object::class_of(t).clone()),
            None => fr.called.clone(),
        };
        // Static closures without bound variables declared inside a function
        // are cached per declaration for the first called scope
        // (`ZEND_DECLARE_LAMBDA_FUNCTION` cache slot). A closure without
        // static variables never uses its function's static slots, so slot 0
        // holds the cached object.
        let cacheable = is_static
            && f.uses.is_empty()
            && f.code.num_statics == 0
            && fr.kind == FrameKind::Function;
        let cached = if cacheable {
            f.statics.borrow().first().cloned().flatten()
        } else {
            None
        };
        if let Some(r) = &cached {
            if let Value::Object(o) = r.get() {
                let same = object::internal::<ClosureData, _>(&o, |d| {
                    same_class(d.called.as_ref(), called.as_ref())
                });
                if same == Some(true) {
                    return Ok(Value::Object(o));
                }
            }
        }
        let func = Func::User(f.clone());
        let data = ClosureData {
            statics: closure_statics(&func, false, Vec::new()),
            func,
            this,
            scope: fr.scope.clone(),
            called,
            vars,
            fake: false,
            magic_name: None,
            partial_inner: None,
        };
        let o = self.new_closure_object(data);
        if cacheable && cached.is_none() {
            let mut st = f.statics.borrow_mut();
            if st.is_empty() {
                st.push(Some(PhpRef::new(Value::Object(o.clone()))));
            }
        }
        Ok(Value::Object(o))
    }

    /// First-class callable syntax (`ZEND_CALLABLE_CONVERT`): wraps a
    /// pending call in a closure. Closures of plain functions are cached and
    /// shared (`EG(callable_convert_cache)`).
    #[allow(clippy::unnecessary_wraps)] // fallible signature expected by the executor
    pub(crate) fn make_closure_from_call(&mut self, call: PendingCall) -> VmResult<Value> {
        if let Some(c) = &call.closure {
            return Ok(Value::Object(c.clone()));
        }
        if let Some(t) = &call.this {
            if is_invoke_trampoline(&call) && object::has_internal::<ClosureData>(t) {
                return Ok(Value::Object(t.clone()));
            }
        }
        let cacheable = call.this.is_none()
            && call.scope.is_none()
            && call.called.is_none()
            && call.magic_name.is_none()
            && !matches!(&call.func, Func::User(u) if u.is_closure());
        let key = ZStr::from(format!(
            "\0fcc\0{}",
            call.func.name().to_string_lossy().to_ascii_lowercase()
        ));
        let cls = if cacheable {
            self.closure_class()
        } else {
            None
        };
        if let Some(cls) = &cls {
            if let Some(v) = cls.enum_cases.borrow().get(&key) {
                return Ok(v.clone());
            }
        }
        let v = self.fake_closure(call);
        if let Some(cls) = &cls {
            // The `Closure` class is never an enum: its case map serves as the
            // per-request FCC cache.
            cls.enum_cases.borrow_mut().insert(key, v.clone());
        }
        Ok(v)
    }

    /// `zend_create_fake_closure` from a resolved call.
    pub(crate) fn fake_closure(&mut self, call: PendingCall) -> Value {
        let data = ClosureData {
            statics: closure_statics(&call.func, true, Vec::new()),
            func: call.func,
            this: call.this,
            scope: call.scope,
            called: call.called,
            vars: Vec::new(),
            fake: true,
            magic_name: call.magic_name,
            partial_inner: None,
        };
        Value::Object(self.new_closure_object(data))
    }

    /// `Z_PARAM_FUNC` for a callable kept past the call that received it
    /// (`zend_fcall_info_cache`): resolves `v` once in the calling user
    /// frame's scope, raising the `self`/`parent`/`static` deprecations
    /// there, and returns a value that calls the resolved function from
    /// any context. Plain function names and `Closure` objects are
    /// returned as they are; other callables become a fake closure bound to
    /// the resolved scope, `$this` and called class. `Ok(Err(reason))` when
    /// `v` is not callable.
    pub fn resolve_callable_value(&mut self, v: &Value) -> VmResult<Result<Value, String>> {
        let call = match self.zend_is_callable(v, false)? {
            Ok(call) => call,
            Err(reason) => return Ok(Err(reason)),
        };
        Ok(Ok(match v.deref() {
            Value::Str(s) if !s.as_bytes().windows(2).any(|w| w == b"::") => v.deref(),
            Value::Object(o) if object::has_internal::<ClosureData>(&o) => Value::Object(o),
            _ => self.fake_closure(call),
        }))
    }

    /// `Closure::fromCallable()`.
    pub(crate) fn closure_from_callable_value(&mut self, v: &Value) -> VmResult<Value> {
        if let Value::Array(a) = v {
            if let (Some(Value::Object(o)), Some(Value::Str(m))) = (
                a.get(&ArrayKey::Int(0)).map(Value::deref),
                a.get(&ArrayKey::Int(1)).map(Value::deref),
            ) {
                if a.len() == 2
                    && m.eq_ignore_ascii_case(b"__invoke")
                    && object::has_internal::<ClosureData>(&o)
                {
                    return Ok(Value::Object(o));
                }
            }
        }
        match self.zend_is_callable(v, false)? {
            Ok(call) => Ok(self.fake_closure(call)),
            Err(err) => Err(self.make_throw(
                "TypeError",
                &format!("Failed to create closure from callable: {err}"),
            )),
        }
    }

    /// Fast-path pending call for invoking a `Closure` wrapping a `UserFunction`.
    /// Avoids generic dynamic method lookups (`__invoke`) and dispatches directly
    /// to the internal user function.
    #[allow(clippy::unused_self)]
    pub(crate) fn closure_user_call(&self, o: &Obj) -> Option<PendingCall> {
        if object::class_of(o).lc_name.as_bytes() != b"closure" {
            return None;
        }
        object::internal::<ClosureData, _>(o, |d| {
            if !matches!(&d.func, Func::User(_)) {
                return None;
            }
            Some(PendingCall {
                this: d.this.clone(),
                scope: d.scope.clone(),
                called: d.called.clone(),
                closure: if d.fake { None } else { Some(o.clone()) },
                magic_name: d.magic_name.clone(),
                ..PendingCall::new(d.func.clone())
            })
        })
        .flatten()
    }

    /// The closure data of a `Closure` object.
    #[allow(clippy::unused_self)] // method-style API used across the VM
    pub(crate) fn closure_data(&self, o: &Obj) -> Option<ClosureData> {
        object::internal::<ClosureData, _>(o, |d| d.clone())
    }

    /// A pending call for invoking a closure object. Built from a borrow of
    /// the closure's data: a copy would clone the bound variables and
    /// release the clones afterwards, which php-src never does here (a
    /// released reference handle would be buffered as a GC possible root).
    #[allow(clippy::unused_self)] // method-style API used across the VM
    pub(crate) fn closure_call(&self, o: &Obj) -> Option<PendingCall> {
        if object::class_of(o).lc_name.as_bytes() != b"closure" {
            return None;
        }
        object::internal::<ClosureData, _>(o, |d| PendingCall {
            this: d.this.clone(),
            scope: d.scope.clone(),
            called: d.called.clone(),
            closure: if d.fake { None } else { Some(o.clone()) },
            magic_name: d.magic_name.clone(),
            ..PendingCall::new(d.func.clone())
        })
    }

    /// `$closure->__invoke(...)` (`zend_get_closure_invoke_method`): a
    /// `Closure::__invoke` trampoline with the closure's parameters.
    pub(crate) fn closure_method_call(&self, o: &Obj, lc: &ZStr) -> Option<PendingCall> {
        if lc.as_bytes() != b"__invoke" {
            return None;
        }
        if let Some(mut c) = self.closure_user_call(o) {
            c.magic_name = Some(ZStr::from("__invoke"));
            return Some(c);
        }
        let d = self.closure_data(o)?;
        let mut params: Vec<ParamInfo> = d
            .func
            .params()
            .iter()
            .map(|p| ParamInfo {
                ty: None,
                has_default: true,
                ..p.clone()
            })
            .collect();
        if !d.func.is_variadic() {
            params.push(ParamInfo {
                name: ZStr::from("args"),
                by_ref: false,
                variadic: true,
                ty: None,
                has_default: true,
                default_text: None,
                promote: 0,
                sensitive: false,
                default_expr: None,
                attributes: Vec::new(),
            });
        }
        let tramp = Builtin::new(
            ZStr::from("__invoke"),
            crate::func::Signature {
                defaults: vec![None; params.len()],
                params,
                required: 0,
                return_type: None,
            },
            flags::PUBLIC | flags::VARIADIC | flags::CLOSURE | (d.func.flags() & flags::RETURN_REF),
            closure_invoke,
        );
        let cls = self.closure_class();
        Some(PendingCall {
            this: Some(o.clone()),
            scope: cls.clone(),
            called: cls,
            closure: Some(o.clone()),
            magic_name: Some(ZStr::from("__invoke")),
            ..PendingCall::new(Func::Builtin(Rc::new(tramp)))
        })
    }

    /// Binds captured variables and statics into the new frame at index
    /// `frame` of the frame stack.
    ///
    /// The closure's data is read through a shared borrow found from the
    /// object's header once (it is only read here), not through
    /// `object::internal`'s second lookup and mutable borrow: this runs on
    /// every call of a closure.
    ///
    /// The static-variable store is attached only when the function
    /// declares static variables: without them no op reads it
    /// ([`Vm::static_slot`], [`Vm::static_init`]), and a frame left without
    /// one returns without `Frame::clear_rare_handles` (an out-of-line call
    /// and a reference-count round trip per call). Inlined into the call
    /// paths (`push_simple_ucall`, `push_user_call`).
    #[inline]
    pub(crate) fn bind_closure_vars(&mut self, frame: usize, c: &Obj) {
        let Some(f) = self.frames.get_mut(frame) else {
            return;
        };
        let internal = object::data(c).internal.borrow();
        let Some(d) = internal
            .as_ref()
            .and_then(|x| x.downcast_ref::<ClosureData>())
        else {
            return;
        };
        if f.this.is_none() && d.this.is_some() {
            f.this = d.this.clone();
        }
        if f.scope.is_none() && d.scope.is_some() {
            f.scope = d.scope.clone();
        }
        if f.called.is_none() && d.called.is_some() {
            f.called = d.called.clone();
        }
        if f.magic_name.is_none() && d.magic_name.is_some() {
            f.magic_name = d.magic_name.clone();
        }
        for (cv, v) in &d.vars {
            if let Some(slot) = f.cvs.get_mut(*cv as usize) {
                slot.set(Some(v.clone()));
            }
        }
        if has_statics(&d.func) {
            f.statics = Some(d.statics.clone());
        }
    }

    fn statics_store(&mut self) -> Rc<RefCell<Vec<Option<PhpRef>>>> {
        if let Some(s) = &self.frame().statics {
            return s.clone();
        }
        let s = Rc::new(RefCell::new(Vec::new()));
        self.frame_mut().statics = Some(s.clone());
        s
    }

    /// Static variable slot `idx`, if initialized.
    pub(crate) fn static_slot(&mut self, idx: u32) -> Option<PhpRef> {
        if self.frame().statics.is_none() {
            if let Some(uf) = self.frame().ufunc.clone() {
                return uf.statics.borrow().get(idx as usize).cloned().flatten();
            }
        }
        let s = self.statics_store();
        let b = s.borrow();
        b.get(idx as usize).cloned().flatten()
    }

    /// Initializes static variable slot `idx`. A slot that a recursive call
    /// made from the initializer already set wins: `ZEND_BIND_STATIC` binds
    /// the existing reference and drops the computed value
    /// (`Zend/tests/static_variables/static_variables_recursive.phpt`).
    pub(crate) fn static_init(&mut self, idx: u32, v: Value) -> PhpRef {
        fn put(b: &mut Vec<Option<PhpRef>>, idx: usize, v: Value) -> PhpRef {
            if b.len() <= idx {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "bounded: static variable slot of the compiled function"
                )]
                b.resize(idx + 1, None);
            }
            b[idx].get_or_insert_with(|| PhpRef::new(v)).clone()
        }
        if self.frame().statics.is_none() {
            if let Some(uf) = self.frame().ufunc.clone() {
                return put(&mut uf.statics.borrow_mut(), idx as usize, v);
            }
        }
        let s = self.statics_store();
        let r = put(&mut s.borrow_mut(), idx as usize, v);
        r
    }

    /// `zend_valid_closure_binding`: whether `d` may be rebound to
    /// `new_this`/`scope`; emits the warning and returns `false` otherwise.
    fn valid_closure_binding(
        &mut self,
        d: &ClosureData,
        new_this: Option<&Obj>,
        scope: Option<&Rc<Class>>,
    ) -> VmResult<bool> {
        let fflags = d.func.flags();
        let is_fake = d.fake || d.func.name().as_bytes().starts_with(b"{closure:pfa:");
        let func_scope = d.scope.as_ref();
        if let Some(t) = new_this {
            if fflags & flags::STATIC != 0 {
                self.warning(
                    "Cannot bind an instance to a static closure, this will be an error in PHP 9",
                )?;
                return Ok(false);
            }
            if is_fake {
                if let Some(fs) = func_scope {
                    if !object::class_of(t).is_subclass_of(fs) {
                        let msg = format!(
                            "Cannot bind method {}::{}() to object of class {}, this will be an error in PHP 9",
                            object::display_name(&fs.name),
                            fake_function_name(d),
                            object::display_name(&object::class_of(t).name)
                        );
                        self.warning(&msg)?;
                        return Ok(false);
                    }
                }
            }
        } else if is_fake && func_scope.is_some() && fflags & flags::STATIC == 0 {
            self.warning("Cannot unbind $this of method, this will be an error in PHP 9")?;
            return Ok(false);
        } else if !is_fake && d.this.is_some() && uses_this(&d.func) {
            self.warning(
                "Cannot unbind $this of closure using $this, this will be an error in PHP 9",
            )?;
            return Ok(false);
        }
        if let Some(s) = scope {
            if s.flags & cflags::INTERNAL != 0 && !same_class(Some(s), func_scope) {
                self.warning(&format!(
                    "Cannot bind closure to scope of internal class {}, this will be an error in PHP 9",
                    s.name
                ))?;
                return Ok(false);
            }
        }
        if is_fake && !same_class(scope, func_scope) {
            self.warning(if func_scope.is_none() {
                "Cannot rebind scope of closure created from function, this will be an error in PHP 9"
            } else {
                "Cannot rebind scope of closure created from method, this will be an error in PHP 9"
            })?;
            return Ok(false);
        }
        Ok(true)
    }

    /// `zend_create_closure_ex` for a rebinding: a new closure object over
    /// the same function with the given scope/called scope/`$this`.
    fn rebound_closure(
        &mut self,
        d: &ClosureData,
        scope: Option<Rc<Class>>,
        called: Option<Rc<Class>>,
        this: Option<Obj>,
    ) -> ClosureData {
        let mut scope = scope;
        let mut this = this;
        if scope.is_none() && this.is_some() {
            // Dummy scope when binding an object without a scope.
            scope = self.closure_class();
        }
        if matches!(d.func, Func::Builtin(_)) && d.scope.is_none() {
            // Free internal functions have no meaningful scope or $this.
            scope = None;
            this = None;
        }
        if scope.is_none() || d.func.flags() & flags::STATIC != 0 {
            this = None;
        }
        // Real closures get a snapshot of the static variables; fake ones
        // keep sharing the function's.
        let statics = if !d.fake && has_statics(&d.func) {
            d.statics
                .borrow()
                .iter()
                .map(|s| s.as_ref().map(|r| PhpRef::new(r.get())))
                .collect()
        } else {
            Vec::new()
        };
        let mut vars = d.vars.clone();
        if let Some(inner_cv) = d.partial_inner {
            if let Some((_, Value::Object(inner))) = vars.iter_mut().find(|(cv, _)| *cv == inner_cv)
            {
                if let Some(inner_data) = self.closure_data(inner) {
                    let rebound = self.rebound_closure(
                        &inner_data,
                        inner_data.scope.clone(),
                        called.clone(),
                        this.clone(),
                    );
                    *inner = self.new_closure_object(rebound);
                }
            }
        }
        ClosureData {
            statics: closure_statics(&d.func, d.fake, statics),
            func: d.func.clone(),
            this,
            scope,
            called,
            vars,
            fake: d.fake,
            magic_name: d.magic_name.clone(),
            partial_inner: d.partial_inner,
        }
    }

    /// `do_closure_bind` (`Closure::bind` / `Closure::bindTo`).
    fn bind_closure(
        &mut self,
        c: &Obj,
        new_this: Option<Obj>,
        new_scope: &Value,
    ) -> VmResult<Value> {
        let Some(d) = self.closure_data(c) else {
            return Ok(Value::Null);
        };
        let scope: Option<Rc<Class>> = match new_scope {
            Value::Object(o) => Some(object::class_of(o).clone()),
            Value::Null => None,
            Value::Str(s) if s.as_bytes() == b"static" => d.scope.clone(),
            v => {
                let s = self.to_zstr(v)?;
                let Some(c) = self.lookup_class(&s)? else {
                    self.warning(&format!("Class \"{}\" not found", s.to_string_lossy()))?;
                    return Ok(Value::Null);
                };
                Some(c)
            }
        };
        if !self.valid_closure_binding(&d, new_this.as_ref(), scope.as_ref())? {
            return Ok(Value::Null);
        }
        let called = match &new_this {
            Some(t) => Some(object::class_of(t).clone()),
            None => scope.clone(),
        };
        let nd = self.rebound_closure(&d, scope, called, new_this);
        Ok(Value::Object(self.new_closure_object(nd)))
    }
}

/// Whether a pending call is the `Closure::__invoke` trampoline.
fn is_invoke_trampoline(call: &PendingCall) -> bool {
    matches!(&call.func, Func::Builtin(b) if b.name.as_bytes() == b"__invoke")
        && call
            .scope
            .as_ref()
            .is_some_and(|s| s.lc_name.as_bytes() == b"closure")
}

/// Whether two optional classes are the same class.
fn same_class(a: Option<&Rc<Class>>, b: Option<&Rc<Class>>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => Rc::ptr_eq(x, y),
        (None, None) => true,
        _ => false,
    }
}

/// `ZEND_ACC_USES_THIS`: whether a user function reads `$this`.
fn uses_this(f: &Func) -> bool {
    let Func::User(u) = f else { return false };
    u.code.ops.iter().any(|op| {
        matches!(
            op,
            Op::FetchThis { .. }
                | Op::InitMethodCall {
                    obj: Operand::Unused,
                    ..
                }
        )
    }) || u
        .code
        .places
        .iter()
        .any(|p| matches!(p.base, PlaceBase::This))
}

/// The function name of a fake closure (`__call` targets use the called
/// method name).
fn fake_function_name(d: &ClosureData) -> String {
    match &d.magic_name {
        Some(m) => m.to_string_lossy(),
        None => d.func.name().to_string_lossy(),
    }
}

/// The name of a closure as reported by `is_callable()` and debug output:
/// `Class::method` for fake closures of methods, else the function name.
pub(crate) fn closure_callable_name(d: &ClosureData) -> String {
    let name = fake_function_name(d);
    match (&d.scope, d.fake) {
        (Some(s), true) => format!("{}::{name}", object::display_name(&s.name)),
        _ => name,
    }
}

fn scope_arg(a: &Args, n: usize) -> Value {
    if a.has(n) {
        a.at(n)
    } else {
        Value::Str(ZStr::from("static"))
    }
}

fn closure_bind(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Value::Object(c) = a.at(0) else {
        return Ok(Value::Null);
    };
    let this = match a.at(1) {
        Value::Object(o) => Some(o),
        _ => None,
    };
    vm.bind_closure(&c, this, &scope_arg(a, 2))
}

fn closure_bind_to(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = a.this_obj();
    let this = match a.at(0) {
        Value::Object(o) => Some(o),
        _ => None,
    };
    vm.bind_closure(&c, this, &scope_arg(a, 1))
}

fn closure_call_method(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = a.this_obj();
    let Some(d) = vm.closure_data(&c) else {
        return Ok(Value::Null);
    };
    let Value::Object(this) = a.at(0) else {
        return Ok(Value::Null);
    };
    let cls = object::class_of(&this).clone();
    if !vm.valid_closure_binding(&d, Some(&this), Some(&cls))? {
        return Ok(Value::Null);
    }
    let args: Vec<Value> = a.values.iter().skip(1).cloned().collect();
    // `zend_closure_call_magic`'s on-stack fake closure: a plain call
    // allocates no object (so no object handle); it runs the closure's
    // function with the new `$this` and scope, its bound variables and
    // static variables. Only a generator gets a real closure object.
    if d.func.flags() & flags::GENERATOR == 0 {
        if let Some(mut call) = vm.closure_user_call(&c) {
            call.this = Some(this);
            call.scope = Some(cls.clone());
            call.called = Some(cls);
            let named = std::mem::take(&mut a.named);
            let r = vm.call_resolved_named(call, args, named)?;
            return Ok(r.deref());
        }
    }
    // The call shares the static variables of the closure.
    let tmp = ClosureData {
        scope: Some(cls.clone()),
        called: Some(cls),
        this: Some(this),
        ..d
    };
    let tmp = vm.new_closure_object(tmp);
    let named = std::mem::take(&mut a.named);
    let r = vm.call_value_named(&Value::Object(tmp), args, named)?;
    Ok(r.deref())
}

fn closure_from_callable(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    if let Value::Object(o) = &v {
        if object::has_internal::<ClosureData>(o) {
            return Ok(v);
        }
    }
    vm.closure_from_callable_value(&v)
}

fn closure_get_current(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    // The frame below this builtin's own frame.
    let n = vm.frames.len();
    let cur = n
        .checked_sub(2)
        .and_then(|i| vm.frames.get(i))
        .filter(|f| f.kind == FrameKind::Function)
        .and_then(|f| f.closure.clone());
    match cur {
        Some(c) => Ok(Value::Object(c)),
        None => Err(vm.make_throw("Error", "Current function is not a closure")),
    }
}

/// The `Closure::__invoke` trampoline: forwards to the closure.
fn closure_invoke(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = a.this_obj();
    let params: Vec<(bool, ZStr)> = a
        .func
        .as_ref()
        .map(|b| {
            b.params
                .iter()
                .map(|p| (p.by_ref, p.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    // `zend_handle_undef_args()` on the trampoline: it copies the arginfo of
    // an internal closure function, so a required parameter skipped by
    // named arguments is reported for `Closure::__invoke()` (user arginfo
    // is left to the closure itself).
    if let Some(last) = a.passed.iter().rposition(|&p| p) {
        if let Some(gap) = a.passed[..last].iter().position(|&p| !p) {
            if let Some(Func::Builtin(inner)) = vm.closure_data(&c).map(|d| d.func) {
                if gap < inner.required as usize {
                    let name = params
                        .get(gap)
                        .map_or_else(String::new, |p| p.1.to_string_lossy());
                    return Err(vm.make_throw(
                        "ArgumentCountError",
                        &format!(
                            "Closure::__invoke(): Argument #{} (${name}) not passed",
                            gap + 1
                        ),
                    ));
                }
            }
        }
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: number of arguments of the call"
    )]
    let mut args = Vec::with_capacity(a.values.len());
    let mut named = std::mem::take(&mut a.named);
    for (i, v) in std::mem::take(&mut a.values).into_iter().enumerate() {
        if !a.passed.get(i).copied().unwrap_or(true) {
            continue;
        }
        let by_ref = params.get(i).is_some_and(|p| p.0);
        let v = if by_ref && !v.is_reference() {
            let name = params
                .get(i)
                .map_or_else(String::new, |p| p.1.to_string_lossy());
            vm.warning(&format!(
                "Closure::__invoke(): Argument #{} (${name}) must be passed by reference, value given",
                i + 1
            ))?;
            Value::Reference(PhpRef::new(v))
        } else {
            v
        };
        if i == args.len() {
            args.push(v);
        } else if let Some((_, name)) = params.get(i) {
            named.push((name.clone(), v));
        }
    }
    vm.call_value_named(&Value::Object(c), args, named)
}

/// Debug information of a closure (`zend_closure_get_debug_info`).
pub(crate) fn closure_debug_array(_vm: &mut Vm, o: &Obj) -> Option<ZArray> {
    object::internal::<ClosureData, _>(o, |d| closure_debug_table(d)).map(ZArray::from_table)
}

fn str_key(s: &str) -> ArrayKey {
    ArrayKey::Str(ZStr::from(s))
}

/// A static variable as shown by `var_dump` (references held only by the
/// closure are shown as plain values).
fn static_display(v: &Value) -> Value {
    match v {
        Value::Reference(r) if r.refcount() == 1 => r.get(),
        other => other.clone(),
    }
}

fn closure_debug_table(d: &ClosureData) -> HashTable {
    let mut t = HashTable::new();
    if d.fake {
        t.insert(
            str_key("function"),
            Value::Str(ZStr::from(closure_callable_name(d))),
        );
    } else {
        t.insert(str_key("name"), Value::Str(d.func.name().clone()));
        if let Func::User(u) = &d.func {
            t.insert(str_key("file"), Value::Str(u.code.filename.clone()));
            t.insert(str_key("line"), Value::Int(i64::from(u.line_start)));
        }
    }
    if let Func::User(u) = &d.func {
        let mut st = HashTable::new();
        if !d.fake {
            for (cv, v) in &d.vars {
                let n = u
                    .code
                    .cv_names
                    .get(*cv as usize)
                    .cloned()
                    .unwrap_or_default();
                st.insert(ArrayKey::Str(n), static_display(v));
            }
        }
        let names = static_var_names(u);
        if !names.is_empty() {
            let store = if d.fake {
                None
            } else {
                Some(d.statics.borrow())
            };
            let fstore = u.statics.borrow();
            let slots: &Vec<Option<PhpRef>> = store.as_deref().unwrap_or(&fstore);
            for (idx, name, init) in names {
                let v = match slots.get(idx as usize).and_then(Option::as_ref) {
                    Some(r) if r.refcount() == 1 => r.get(),
                    Some(r) => Value::Reference(r.clone()),
                    None => init,
                };
                st.insert(ArrayKey::Str(name), v);
            }
        }
        if !st.is_empty() {
            t.insert(str_key("static"), Value::Array(ZArray::from_table(st)));
        }
    }
    if let Some(this) = &d.this {
        t.insert(str_key("this"), Value::Object(this.clone()));
    }
    if d.magic_name.is_none() {
        let params = d.func.params();
        if !params.is_empty() {
            let required = d.func.required() as usize;
            let mut pt = HashTable::new();
            for (i, p) in params.iter().enumerate() {
                let key = format!("{}${}", if p.by_ref { "&" } else { "" }, p.name);
                let desc = if i >= required {
                    "<optional>"
                } else {
                    "<required>"
                };
                pt.insert(ArrayKey::Str(ZStr::from(key)), Value::Str(ZStr::from(desc)));
            }
            t.insert(str_key("parameter"), Value::Array(ZArray::from_table(pt)));
        }
    }
    t
}

/// `static $x` declarations of a function: (slot, name, value before the
/// declaration runs) in slot order, from the function's static variable
/// table (`op_array->static_variables`). Constant initializers are known at
/// compile time; others start as `null`.
fn static_var_names(u: &UserFunction) -> Vec<(u32, ZStr, Value)> {
    u.code
        .static_vars
        .iter()
        .zip(0u32..)
        .map(|(d, idx)| (idx, d.name.clone(), d.init.clone().unwrap_or_default()))
        .collect()
}

/// Registers the `Closure` class.
pub(crate) fn register(vm: &mut Vm) {
    init_no_statics();
    ClassBuilder::new("Closure")
        .flags(cflags::FINAL | cflags::NO_NEW | cflags::NOT_SERIALIZABLE | cflags::NO_DYNAMIC_PROPERTIES)
        .internal(InternalKind::Closure)
        .method(
            "bind(Closure $closure, ?object $newThis, object|string|null $newScope = \"static\"): ?Closure",
            flags::PUBLIC | flags::STATIC,
            closure_bind,
        )
        .method(
            "bindTo(?object $newThis, object|string|null $newScope = \"static\"): ?Closure",
            flags::PUBLIC,
            closure_bind_to,
        )
        .method("call(object $newThis, mixed ...$args): mixed", flags::PUBLIC, closure_call_method)
        // Declared `callable`, but checked by the handler (zpp `Z_PARAM_ZVAL`)
        // to report "Failed to create closure from callable".
        .method(
            "fromCallable(mixed $callback): Closure",
            flags::PUBLIC | flags::STATIC,
            closure_from_callable,
        )
        .method("getCurrent(): Closure", flags::PUBLIC | flags::STATIC, closure_get_current)
        .register(vm);
}

#[cfg(test)]
mod tests;
