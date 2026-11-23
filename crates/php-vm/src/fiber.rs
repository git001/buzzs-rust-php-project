// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Fibers (`Zend/zend_fibers.c`).
//!
//! Each fiber runs on its own native stack ([`stack::Coroutine`]), so a
//! fiber can suspend from anywhere: nested user calls, internal callbacks
//! (`array_map`), generators, destructors, `__get`, ...
//!
//! The VM frame stack (`vm.frames`) is shared. While a fiber runs, its
//! frames sit on top of the resumer's frames starting at a fixed index
//! `base` (indices held by the interpreter loops on the fiber's native stack
//! must stay valid across suspensions). When the resumer's stack is
//! shallower than `base`, invisible filler frames pad it; when it is deeper,
//! the frames just above `{main}` are set aside for the duration of the run
//! (they only matter for backtraces). On suspension the fiber's frames are
//! moved into the fiber object and the resumer's frames are restored.
//!
//! Destroying a suspended fiber (`zend_fiber_object_destroy`) resumes it
//! with an uncatchable "graceful exit" exception that runs `finally` blocks
//! and destructors while unwinding.

mod stack;

use crate::builtins::classbuilder::ClassBuilder;
use crate::class::{cflags, Class};
use crate::func::{flags, Args};
use crate::object;
use crate::observer::FiberContext;
use crate::vm::{Frame, FrameKind, PendingCall, Throw, Vm, VmResult};
use php_runtime::{Obj, Value, ZStr};
use stack::{Coroutine, StackError, Suspender};
use std::cell::RefCell;
use std::rc::Rc;

/// Fiber status (`ZEND_FIBER_STATUS_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberState {
    /// Constructed, not started.
    Init,
    /// Running, or resuming another fiber (`caller != NULL`).
    Running,
    /// Suspended by `Fiber::suspend()`.
    Suspended,
    /// Returned, threw or bailed out.
    Dead,
}

/// The class a fiber object implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FiberKind {
    /// `Fiber`: `Fiber::suspend()` suspends the innermost one
    /// (`EG(active_fiber)`).
    Fiber,
    /// `zend_test`'s `_ZendTestFiber` (`ext/zend_test/fiber.c`), a symmetric
    /// coroutine with its own active-fiber pointer (`ZT_G(active_fiber)`):
    /// suspending either kind from inside the other suspends the contexts
    /// running above it together with it.
    Test,
}

/// Internal state of a `Fiber`.
#[derive(Debug)]
pub struct FiberData {
    /// `Fiber` or `_ZendTestFiber`.
    kind: FiberKind,
    /// The callable resolved by the constructor (`fci`/`fcc`: keeps the
    /// object a `[A::class, 'm']` callable bound to); taken by `start()`.
    call: Option<PendingCall>,
    /// The callable as given to the constructor (`ReflectionFiber::getCallable`).
    pub callable: Value,
    /// Status.
    pub state: FiberState,
    /// `ZEND_FIBER_FLAG_THREW`.
    pub threw: bool,
    /// `ZEND_FIBER_FLAG_BAILOUT`.
    pub bailout: bool,
    /// `ZEND_FIBER_FLAG_DESTROYED`.
    pub destroyed: bool,
    /// The native stack (from `start()` until the fiber terminates).
    co: Option<Rc<Coroutine>>,
    /// Switches back to the resumer (valid while `co` lives).
    suspender: Option<Suspender>,
    /// The fiber's frames while it is suspended.
    pub frames: Vec<Frame>,
    /// Index of the fiber's first frame in `vm.frames`.
    pub base: usize,
    /// `vm.depth` of the fiber's native stack while suspended.
    depth: u32,
    /// `EG(error_reporting)` of the fiber while suspended (part of the VM
    /// state swapped by `zend_fiber_switch_context`).
    error_reporting: i64,
    /// Value (or error) passed by the last switch in either direction.
    transfer: Option<VmResult<Value>>,
    /// Return value.
    pub ret: Value,
    /// Fibers that were running above this one when it was suspended from
    /// inside them (a `Fiber::suspend()` inside a `_ZendTestFiber`, or the
    /// reverse), innermost last; resumed together with it.
    nested: Vec<Obj>,
    /// `_ZendTestFiber::pipeTo()`: started with this fiber's return value
    /// when it returns.
    target: Option<Obj>,
    /// Whether this is the target of another fiber's `pipeTo()`
    /// (`fiber->previous != NULL` before `start()`).
    is_target: bool,
}

impl FiberData {
    fn new(call: PendingCall, callable: Value, kind: FiberKind) -> Self {
        FiberData {
            kind,
            call: Some(call),
            callable,
            state: FiberState::Init,
            threw: false,
            bailout: false,
            destroyed: false,
            co: None,
            suspender: None,
            frames: Vec::new(),
            base: 0,
            depth: 0,
            error_reporting: 0,
            transfer: None,
            ret: Value::Null,
            nested: Vec::new(),
            target: None,
            is_target: false,
        }
    }
}

fn with<R>(o: &Obj, f: impl FnOnce(&mut FiberData) -> R) -> Option<R> {
    object::internal::<FiberData, R>(o, f)
}

/// Extra room left below a fiber's frames so that it can later be resumed
/// from a deeper call stack without hiding frames from backtraces.
const SLACK: usize = 32;

/// `ZEND_FIBER_DEFAULT_C_STACK_SIZE`.
const DEFAULT_STACK: usize = 4096 * 512;

/// Our interpreter needs more native stack per PHP-level nesting than
/// php-src does; scale `fiber.stack_size` by this factor.
pub(crate) const STACK_SCALE: usize = 4;

/// Conservative native stack use of one nested interpreter loop, used to
/// turn the stack size into a nesting budget (`vm.depth`) so that deep
/// recursion fails with an error instead of hitting the guard page.
const BYTES_PER_LEVEL: usize = 64 * 1024;

/// Default `error_reporting` (`E_ALL`).
const E_ALL: i64 = 30719;

/// Nesting limit enforced by `Vm::execute`.
const MAX_DEPTH: u32 = 20000;

thread_local! {
    static GRACEFUL_EXIT: RefCell<Option<Rc<Class>>> = const { RefCell::new(None) };
}

/// Whether `o` is the internal "graceful exit" unwinding object
/// (`zend_is_graceful_exit`), which no `catch` block matches.
pub(crate) fn is_graceful_exit(o: &Obj) -> bool {
    GRACEFUL_EXIT.with(|g| {
        g.borrow()
            .as_ref()
            .is_some_and(|c| Rc::ptr_eq(c, object::class_of(o)))
    })
}

/// `builtin_name` of the code-less frame standing for a fiber's bottom
/// frame (`zend_fiber_function`) when it is started by the engine itself
/// rather than by `Fiber->start()` (`zend_fiber_start` from the collector):
/// it attributes the fiber's first call to "[internal function]" and shows
/// no entry of its own.
const BOTTOM_FRAME: &str = "{fiber}";

/// `builtin_name` of the bottom frame of a suspended fiber resumed by its
/// destruction: `Fiber::suspend()` clears `stack_bottom->prev_execute_data`
/// and `zend_fiber_object_destroy` does not set it again, so backtraces
/// taken while the fiber unwinds end there (see [`ends_backtrace`]).
const DETACHED_FRAME: &str = "{detached fiber}";

/// Whether backtraces stop at `f` (the bottom of a fiber whose
/// `prev_execute_data` is `NULL`).
pub(crate) fn ends_backtrace(f: &Frame) -> bool {
    f.kind == FrameKind::Builtin
        && f.func.is_none()
        && f.builtin_name
            .as_ref()
            .is_some_and(|n| n.as_bytes() == DETACHED_FRAME.as_bytes())
}

pub(crate) fn create_graceful_exit() -> Option<Obj> {
    let cls = GRACEFUL_EXIT.with(|g| g.borrow().clone())?;
    Some(object::new_object(cls, Vec::new()))
}

/// How the resumer's frames were adjusted to put the fiber's frames at
/// their base index.
enum Prefix {
    /// Nothing to do.
    Exact,
    /// `n` filler frames were inserted at `pos`.
    Fillers { pos: usize, n: usize },
    /// These frames were removed from index 1.
    Stash(Vec<Frame>),
}

/// The observer view of `fiber`'s context, with `caller` as `fiber->caller`.
fn context_of(fiber: &Obj, caller: Option<usize>) -> FiberContext {
    let (status, destroyed, threw, kind) = with(fiber, |d| (d.state, d.destroyed, d.threw, d.kind))
        .unwrap_or((FiberState::Init, false, false, FiberKind::Fiber));
    FiberContext {
        id: fiber.addr(),
        is_fiber: kind == FiberKind::Fiber,
        status,
        destroyed,
        threw,
        caller,
    }
}

impl Vm {
    /// Identity of the context running with `depth` fibers active: the
    /// main context (`EG(main_fiber_context)`) for 0.
    fn fiber_context_id(&self, depth: usize) -> usize {
        depth
            .checked_sub(1)
            .and_then(|i| self.fibers.get(i))
            .map_or(std::ptr::from_ref(self) as usize, Obj::addr)
    }

    /// The observer view of the context running with `depth` fibers active
    /// (a running fiber's caller is the context that resumed it).
    fn fiber_stack_context(&self, depth: usize) -> FiberContext {
        match depth.checked_sub(1).and_then(|i| self.fibers.get(i)) {
            Some(f) => context_of(f, Some(self.fiber_context_id(depth - 1))),
            None => FiberContext {
                id: self.fiber_context_id(0),
                is_fiber: false,
                status: FiberState::Running,
                destroyed: false,
                threw: false,
                caller: None,
            },
        }
    }

    /// Indices in `vm.frames` of the first frame of each running fiber,
    /// outermost first (where php-src's fiber stacks start with their
    /// `{main}`-like bottom frame, `zend_fiber_execute`).
    #[must_use]
    pub fn running_fiber_bases(&self) -> Vec<usize> {
        self.fibers
            .iter()
            .filter_map(|f| with(f, |d| d.base))
            .collect()
    }

    fn fiber_error(&mut self, msg: &str) -> Throw {
        self.make_throw("FiberError", msg)
    }

    /// A frame that backtraces skip but whose location is that of `model`.
    fn filler_frame(&self, model: &Frame) -> Frame {
        let kind = if model.kind == FrameKind::Builtin {
            FrameKind::Builtin
        } else {
            FrameKind::ConstExpr
        };
        let mut f = Frame::with_vars(self.empty_code.clone(), kind, self.empty_vars.clone(), None);
        f.code = model.code.clone();
        f.ip = model.ip;
        f
    }

    /// Makes `self.frames.len() == base`, keeping the topmost frame (the
    /// `Fiber->start()`/`resume()` call) on top.
    fn fiber_install_prefix(&mut self, base: usize) -> Prefix {
        let d = self.frames.len();
        if d == 0 || base < 2 {
            // Nothing to keep in place: pad (or leave as is).
            if d < base {
                let model = self.frames.last().map(|m| self.filler_frame(m));
                let n = base - d;
                for _ in 0..n {
                    let f = match &model {
                        Some(m) => self.filler_frame(m),
                        None => Frame::with_vars(
                            self.empty_code.clone(),
                            FrameKind::ConstExpr,
                            self.empty_vars.clone(),
                            None,
                        ),
                    };
                    self.frames.insert(d.saturating_sub(1), f);
                }
                return Prefix::Fillers {
                    pos: d.saturating_sub(1),
                    n,
                };
            }
            return Prefix::Exact;
        }
        match d.cmp(&base) {
            std::cmp::Ordering::Equal => Prefix::Exact,
            std::cmp::Ordering::Less => {
                let n = base - d;
                let pos = d - 1;
                let model_idx = d.saturating_sub(2);
                let fillers: Vec<Frame> = (0..n)
                    .map(|_| self.filler_frame(&self.frames[model_idx]))
                    .collect();
                self.frames.splice(pos..pos, fillers);
                Prefix::Fillers { pos, n }
            }
            std::cmp::Ordering::Greater => {
                let n = d - base;
                Prefix::Stash(self.frames.drain(1..=n).collect())
            }
        }
    }

    fn fiber_restore_prefix(&mut self, p: Prefix) {
        match p {
            Prefix::Exact => {}
            Prefix::Fillers { pos, n } => {
                let end = (pos + n).min(self.frames.len());
                self.frames.drain(pos.min(end)..end);
            }
            Prefix::Stash(v) => {
                let at = 1.min(self.frames.len());
                self.frames.splice(at..at, v);
            }
        }
    }

    /// `zend_fiber_resume_internal`: switches into `fiber`, passing `input`
    /// (the value `Fiber::suspend()` returns, or the error it throws), and
    /// returns what the fiber passes back when it suspends or terminates.
    fn fiber_switch_in(&mut self, fiber: &Obj, input: VmResult<Value>) -> VmResult<Value> {
        if self.observer.enabled && with(fiber, |d| d.co.is_some()).unwrap_or(false) {
            // `zend_fiber_switch_context`: notified before the statuses change.
            let from = self.fiber_stack_context(self.fibers.len());
            let to = context_of(fiber, Some(from.id));
            self.observer_fiber_switch(&from, &to);
        }
        let Some((co, base, frames, depth, er, nested)) = with(fiber, |d| {
            d.state = FiberState::Running;
            d.transfer = Some(input);
            (
                d.co.clone(),
                d.base,
                std::mem::take(&mut d.frames),
                d.depth,
                d.error_reporting,
                std::mem::take(&mut d.nested),
            )
        }) else {
            return Ok(Value::Null);
        };
        let Some(co) = co else {
            return Ok(Value::Null);
        };
        let prefix = self.fiber_install_prefix(base);
        self.frames.extend(frames);
        let saved_depth = std::mem::replace(&mut self.depth, depth);
        let saved_er = std::mem::replace(&mut self.error_reporting, er);
        let active = self.fibers.len();
        self.fibers.push(fiber.clone());
        self.fibers.extend(nested);
        // The fiber runs on its own native stack: stack-limit checks
        // measure from its first execution there (approximate on resume).
        let fiber_stack = match self.ini.typed("fiber.stack_size") {
            php_support::ini::IniTyped::Long(n) => usize::try_from(n).unwrap_or(DEFAULT_STACK),
            _ => DEFAULT_STACK,
        }
        .saturating_mul(STACK_SCALE)
        .max(1 << 20);
        let saved_stack = (
            std::mem::replace(&mut self.native_stack_base, 0),
            std::mem::replace(&mut self.native_stack_size, fiber_stack),
        );
        let finished = co.resume();
        self.native_stack_base = saved_stack.0;
        self.native_stack_size = saved_stack.1;
        // `fiber` and, when it was suspended from inside fibers it started
        // (of the other kind), those fibers.
        let mut nested = self.fibers.split_off(active.min(self.fibers.len()));
        let nested = if nested.is_empty() {
            nested
        } else {
            nested.split_off(1)
        };
        let fiber_depth = std::mem::replace(&mut self.depth, saved_depth);
        let fiber_er = std::mem::replace(&mut self.error_reporting, saved_er);
        let frames = if self.frames.len() > base {
            self.frames.split_off(base)
        } else {
            Vec::new()
        };
        self.fiber_restore_prefix(prefix);
        let out = with(fiber, |d| {
            if finished {
                d.state = FiberState::Dead;
                d.co = None;
                d.suspender = None;
            } else {
                d.frames = frames;
                d.depth = fiber_depth;
                d.error_reporting = fiber_er;
                d.nested = nested;
            }
            d.transfer.take()
        })
        .flatten();
        drop(co);
        if finished && self.observer.enabled {
            // `zend_fiber_destroy_context`, after switching back.
            self.observer_fiber_lifecycle(&context_of(fiber, None), true);
        }
        out.unwrap_or(Ok(Value::Null))
    }

    /// A code-less internal frame named `name` (no backtrace entry).
    fn hidden_frame(&self, name: &str) -> Frame {
        let mut f = Frame::with_vars(
            self.empty_code.clone(),
            FrameKind::Builtin,
            self.empty_vars.clone(),
            None,
        );
        f.builtin_name = Some(ZStr::from(name));
        f
    }

    /// `zend_fiber_start` called by the engine (the collector's destructor
    /// fiber, `gc_create_destructor_fiber`): starts `fiber` without a
    /// `Fiber->start()` frame. The fiber's bottom frame links to the
    /// current frame, so its backtraces continue with the caller (e.g.
    /// `gc_collect_cycles()`). Returns what the fiber passes back when it
    /// suspends or terminates.
    pub(crate) fn fiber_start_internal(&mut self, fiber: &Obj) -> VmResult<Value> {
        if state_of(fiber) != FiberState::Init {
            return Err(self.fiber_error("Cannot start a fiber that has already been started"));
        }
        let bottom = self.hidden_frame(BOTTOM_FRAME);
        self.frames.push(bottom);
        let depth = self.frames.len();
        let r = self
            .fiber_create_context(fiber, Vec::new(), Vec::new())
            .and_then(|()| self.fiber_switch_in(fiber, Ok(Value::Null)));
        self.frames.truncate(depth - 1);
        r
    }

    /// Adds the `start()` arguments to the resolved call (the named
    /// argument rules of `zend_call_function`).
    fn fiber_bind_args(
        &mut self,
        mut call: PendingCall,
        args: Vec<Value>,
        named: Vec<(ZStr, Value)>,
    ) -> VmResult<PendingCall> {
        call.args = args.into_iter().map(Some).collect();
        for (k, v) in named {
            let pos = if call.magic_name.is_some() {
                None
            } else {
                call.func
                    .params()
                    .iter()
                    .position(|p| p.name == k && !p.variadic)
            };
            if let Some(p) = pos {
                if call.args.len() <= p {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "bounded: position of a declared parameter"
                    )]
                    call.args.resize(p + 1, None);
                } else if call.args[p].is_some() {
                    return Err(self.make_throw(
                        "Error",
                        &format!(
                            "Named parameter ${} overwrites previous argument",
                            k.to_string_lossy()
                        ),
                    ));
                }
                call.args[p] = Some(v);
            } else {
                if !call.func.is_variadic() && call.magic_name.is_none() {
                    return Err(self.make_throw(
                        "Error",
                        &format!("Unknown named parameter ${}", k.to_string_lossy()),
                    ));
                }
                call.named.push((k, v));
            }
            call.has_named = true;
        }
        Ok(call)
    }

    /// Creates the native stack of a fiber about to start.
    fn fiber_create_context(
        &mut self,
        fiber: &Obj,
        args: Vec<Value>,
        named: Vec<(ZStr, Value)>,
    ) -> VmResult<()> {
        let requested = match self.ini.typed("fiber.stack_size") {
            php_support::ini::IniTyped::Long(n) => usize::try_from(n).unwrap_or(0),
            _ => DEFAULT_STACK,
        };
        if requested < stack::MIN_STACK {
            return Err(self.make_throw(
                "Exception",
                &format!(
                    "Fiber stack size is too small, it needs to be at least {} bytes",
                    stack::MIN_STACK
                ),
            ));
        }
        let size = requested.saturating_mul(STACK_SCALE).max(1 << 20);
        let Some(call) = with(fiber, |d| d.call.take()).flatten() else {
            return Err(self.fiber_error("Cannot start a fiber that has already been started"));
        };
        let call = self.fiber_bind_args(call, args, named)?;
        let vm_ptr: *mut Vm = self;
        let body = Box::new(move |s: &Suspender| {
            // SAFETY: the body only runs inside `Coroutine::resume`, called
            // by `fiber_switch_in` while it holds `&mut Vm` and does not use
            // it until `resume` returns: control is handed back and forth,
            // never shared. The VM does not move while scripts run.
            let vm = unsafe { &mut *vm_ptr };
            fiber_body(vm, *s, call);
        });
        match Coroutine::new(size, body) {
            Ok(co) => {
                let budget = u32::try_from(size / BYTES_PER_LEVEL)
                    .unwrap_or(MAX_DEPTH)
                    .min(MAX_DEPTH);
                let base = self.frames.len() + SLACK;
                // Fibers start with the INI error_reporting, not a value
                // lowered by a `@` around `start()`.
                let er = match self.ini.get("error_reporting") {
                    Some(v) if !v.is_empty() => std::str::from_utf8(v)
                        .ok()
                        .and_then(|s| s.trim().parse::<i64>().ok())
                        .unwrap_or(0),
                    _ => E_ALL,
                };
                with(fiber, |d| {
                    d.co = Some(Rc::new(co));
                    d.base = base;
                    d.depth = MAX_DEPTH - budget;
                    d.error_reporting = er;
                });
                if self.observer.enabled {
                    // `zend_fiber_init_context`.
                    self.observer_fiber_lifecycle(&context_of(fiber, None), false);
                }
                Ok(())
            }
            Err(StackError::TooSmall) => Err(self.make_throw(
                "Exception",
                &format!(
                    "Fiber stack size is too small, it needs to be at least {} bytes",
                    stack::MIN_STACK
                ),
            )),
            Err(StackError::Mmap(m)) => Err(self.make_throw(
                "Exception",
                &format!("Fiber stack allocate failed: mmap failed: {m}"),
            )),
            Err(StackError::Unsupported) => {
                Err(self.fiber_error("Fibers are not supported on this platform"))
            }
        }
    }
}

/// `zend_fiber_execute`: runs on the fiber's own stack.
fn fiber_body(vm: &mut Vm, s: Suspender, call: PendingCall) {
    // No strong reference to the fiber may live on its own stack, or an
    // unreferenced suspended fiber would never be destroyed.
    let Some(kind) = vm.fibers.last().and_then(|f| {
        with(f, |d| {
            d.suspender = Some(s);
            d.kind
        })
    }) else {
        return;
    };
    // `zend_test_fiber_execute` starts its VM stack with a zeroed frame
    // without `prev_execute_data`: backtraces end inside the fiber.
    let bottom = (kind == FiberKind::Test).then(|| {
        vm.frames.push(Frame::with_vars(
            vm.empty_code.clone(),
            FrameKind::Main,
            vm.empty_vars.clone(),
            None,
        ));
        vm.frames.len()
    });
    let r = vm.call_pending(call);
    if let Some(n) = bottom {
        vm.frames.truncate(n - 1);
    }
    let Some(fiber) = vm.fibers.last().cloned() else {
        return;
    };
    let r = release_callable(vm, &fiber, r);
    let destroyed = with(&fiber, |d| d.destroyed).unwrap_or(false);
    let out = match r {
        Ok(v) => {
            with(&fiber, |d| d.ret = v.deref());
            Ok(Value::Null)
        }
        Err(Throw::Exception(e)) if destroyed && is_graceful_exit(&e) => Ok(Value::Null),
        Err(Throw::Exception(e)) => {
            with(&fiber, |d| d.threw = true);
            Err(Throw::Exception(e))
        }
        Err(Throw::Fatal) => {
            with(&fiber, |d| d.bailout = true);
            Err(Throw::Fatal)
        }
        Err(e) => Err(e),
    };
    with(&fiber, |d| {
        d.transfer = Some(out);
        d.state = FiberState::Dead;
    });
    if vm.observer.enabled {
        // The final switch back to the caller (`zend_fiber_trampoline`).
        let depth = vm.fibers.len();
        let from = context_of(&fiber, None);
        let to = vm.fiber_stack_context(depth - 1);
        vm.observer_fiber_switch(&from, &to);
    }
}

/// `zend_fiber_release_callable`, still inside the fiber once its function
/// returned: the callable (a closure with its bound variables, the object
/// of `[$obj, 'm']`) is released, so the destructors it triggers — and a
/// collection when the released values fill the root buffer — run in the
/// fiber; what they throw is thrown by the fiber (chained to the exception
/// the function threw, `zend_objects_destroy_object`).
fn release_callable(vm: &mut Vm, fiber: &Obj, r: VmResult<Value>) -> VmResult<Value> {
    let Some(callable) = with(fiber, |d| std::mem::take(&mut d.callable)) else {
        return r;
    };
    // A function name owns nothing (the collector's destructor fiber).
    if !matches!(callable, Value::Object(_) | Value::Array(_)) {
        return r;
    }
    crate::gc::release_value(&callable);
    drop(callable);
    let mut r = r;
    if object::has_pending_destructors() {
        r = merge_release_error(r, vm.run_pending_destructors());
    }
    if crate::gc::wants_run() && !crate::gc::running() {
        r = merge_release_error(r, vm.gc_safe_point());
    }
    r
}

/// Combines the outcome of a fiber's function with an error raised while
/// releasing its callable.
fn merge_release_error(r: VmResult<Value>, e: VmResult<()>) -> VmResult<Value> {
    match (r, e) {
        (r, Ok(())) => r,
        (Err(Throw::Exception(old)), Err(Throw::Exception(new))) => {
            if !is_graceful_exit(&old) {
                crate::exceptions::chain_previous(&new, &old);
            }
            Err(Throw::Exception(new))
        }
        (Ok(_), Err(e)) | (Err(Throw::Exception(_)), Err(e)) => Err(e),
        (Err(t), Err(_)) => Err(t),
    }
}

fn state_of(o: &Obj) -> FiberState {
    with(o, |d| d.state).unwrap_or(FiberState::Init)
}

fn fiber_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    let callable = a.at(0);
    // `Z_PARAM_FUNC` runs before the "twice" check.
    let call = resolve_fiber_callable(vm, a, &callable)?;
    if object::has_internal::<FiberData>(&o) {
        return Err(vm.fiber_error("Cannot call constructor twice"));
    }
    object::set_internal(&o, FiberData::new(call, callable, FiberKind::Fiber));
    Ok(Value::Null)
}

/// `Z_PARAM_FUNC(fiber->fci, fiber->fci_cache)`: `zend_fcall_info_init`
/// resolves the callable once, in the constructing user frame's scope,
/// raising its deprecations ("Use of \"parent\" in callables …",
/// `["C", "P::m"]` forms) there; the fiber calls that resolution from
/// wherever it is started (`zend_call_function` with the cache, a
/// `ZEND_CALL_DYNAMIC` call). The `callable` parameter type already
/// rejected what does not resolve, unless resolving it again failed.
fn resolve_fiber_callable(vm: &mut Vm, a: &Args, callable: &Value) -> VmResult<PendingCall> {
    match vm.resolve_callable(callable)? {
        Ok(mut call) => {
            call.dynamic = true;
            Ok(call)
        }
        Err(reason) => Err(a.type_error(vm, 1, &format!("must be a valid callback, {reason}"))),
    }
}

/// `zend_fiber_switch_blocked()` (ticks, signal handlers).
fn check_switch(vm: &mut Vm) -> VmResult<()> {
    if vm.fiber_switch_block > 0 {
        return Err(vm.fiber_error("Cannot switch fibers in current execution context"));
    }
    Ok(())
}

fn fiber_start(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    check_switch(vm)?;
    let o = a.this_obj();
    if state_of(&o) != FiberState::Init {
        return Err(vm.fiber_error("Cannot start a fiber that has already been started"));
    }
    let args: Vec<Value> = a.values.clone();
    let named = std::mem::take(&mut a.named);
    vm.fiber_create_context(&o, args, named)?;
    vm.fiber_switch_in(&o, Ok(Value::Null))
}

fn fiber_resume(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    check_switch(vm)?;
    let o = a.this_obj();
    if state_of(&o) != FiberState::Suspended {
        return Err(vm.fiber_error("Cannot resume a fiber that is not suspended"));
    }
    let v = a.get(0).unwrap_or(Value::Null);
    vm.fiber_switch_in(&o, Ok(v))
}

fn fiber_throw(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    check_switch(vm)?;
    let o = a.this_obj();
    let Value::Object(e) = a.at(0) else {
        return Ok(Value::Null);
    };
    if state_of(&o) != FiberState::Suspended {
        return Err(vm.fiber_error("Cannot resume a fiber that is not suspended"));
    }
    vm.fiber_switch_in(&o, Err(Throw::Exception(e)))
}

fn fiber_get_return(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    let Some((state, threw, bailout, ret)) =
        with(&o, |d| (d.state, d.threw, d.bailout, d.ret.clone()))
    else {
        return Err(vm.fiber_error("Cannot get fiber return value: The fiber has not been started"));
    };
    let msg = match state {
        FiberState::Dead if threw => "The fiber threw an exception",
        FiberState::Dead if bailout => "The fiber exited with a fatal error",
        FiberState::Dead => return Ok(ret),
        FiberState::Init => "The fiber has not been started",
        _ => "The fiber has not returned",
    };
    Err(vm.fiber_error(&format!("Cannot get fiber return value: {msg}")))
}

#[allow(clippy::unnecessary_wraps)] // builtin signature
fn fiber_is_started(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(state_of(&a.this_obj()) != FiberState::Init))
}
#[allow(clippy::unnecessary_wraps)] // builtin signature
fn fiber_is_suspended(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(
        state_of(&a.this_obj()) == FiberState::Suspended,
    ))
}
#[allow(clippy::unnecessary_wraps)] // builtin signature
fn fiber_is_running(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(state_of(&a.this_obj()) == FiberState::Running))
}
#[allow(clippy::unnecessary_wraps)] // builtin signature
fn fiber_is_terminated(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(state_of(&a.this_obj()) == FiberState::Dead))
}

/// Index in `vm.fibers` of the innermost running fiber of `kind`
/// (`EG(active_fiber)` / `ZT_G(active_fiber)`).
fn innermost(vm: &Vm, kind: FiberKind) -> Option<usize> {
    vm.fibers
        .iter()
        .rposition(|f| with(f, |d| d.kind == kind).unwrap_or(false))
}

fn fiber_suspend(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.get(0).unwrap_or(Value::Null);
    suspend_innermost(vm, v, FiberKind::Fiber)
}

/// `Fiber::suspend()` / `_ZendTestFiber::suspend()`: suspends the innermost
/// fiber of `kind` together with any fibers of the other kind running
/// above it, which are resumed with it (`fiber->previous`).
fn suspend_innermost(vm: &mut Vm, v: Value, kind: FiberKind) -> VmResult<Value> {
    let Some(index) = innermost(vm, kind) else {
        return Err(vm.fiber_error("Cannot suspend outside of a fiber"));
    };
    let fiber = vm.fibers[index].clone();
    if kind == FiberKind::Fiber && with(&fiber, |d| d.destroyed).unwrap_or(false) {
        return Err(vm.fiber_error("Cannot suspend in a force-closed fiber"));
    }
    check_switch(vm)?;
    if vm.observer.enabled {
        // `zend_fiber_suspend_internal` clears `caller` before switching.
        let from = context_of(&fiber, None);
        let to = vm.fiber_stack_context(index);
        vm.observer_fiber_switch(&from, &to);
    }
    let Some(s) = with(&fiber, |d| {
        d.transfer = Some(Ok(v));
        d.state = FiberState::Suspended;
        d.suspender
    })
    .flatten() else {
        return Err(vm.fiber_error("Cannot switch fibers in current execution context"));
    };
    // Do not keep the fiber alive from its own stack while suspended.
    drop(fiber);
    s.suspend();
    // Resumed: the same fiber is again the innermost of its kind.
    innermost(vm, kind)
        .and_then(|i| with(&vm.fibers[i], |d| d.transfer.take()).flatten())
        .unwrap_or(Ok(Value::Null))
}

#[allow(clippy::unnecessary_wraps)] // builtin signature
fn fiber_get_current(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(
        innermost(vm, FiberKind::Fiber)
            .map_or(Value::Null, |i| Value::Object(vm.fibers[i].clone())),
    )
}

/// `zend_fiber_object_destroy` (the `dtor_obj` handler): unwinds a
/// suspended fiber.
fn fiber_destroy(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    if state_of(&o) != FiberState::Suspended {
        return Ok(Value::Null);
    }
    let Some(g) = create_graceful_exit() else {
        return Ok(Value::Null);
    };
    with(&o, |d| d.destroyed = true);
    // php-src destroys from the `dtor_obj` handler, without a call frame:
    // replace our `__destruct` frame by an internal call-site boundary.
    // Its name stops caller lookup, but no function means no trace entry.
    // The fiber was suspended, which cleared its link to the resumer
    // (`stack_bottom->prev_execute_data = NULL`), and destruction resumes
    // it without setting a new one: its backtraces end at this frame.
    let own = vm.frames.pop();
    let hidden = vm.hidden_frame(DETACHED_FRAME);
    vm.frames.push(hidden);
    let r = vm.fiber_switch_in(&o, Err(Throw::Exception(g)));
    vm.frames.pop();
    if let Some(f) = own {
        vm.frames.push(f);
    }
    if let Err(Throw::Exception(e)) = &r {
        crate::generator::strip_graceful_previous(e);
    }
    r.map(|_| Value::Null)
}

fn fiber_error_construct(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Err(vm.make_throw(
        "Error",
        "The \"FiberError\" class is reserved for internal use and cannot be manually instantiated",
    ))
}

/// Registers `Fiber` and `FiberError`.
/// `zend_fiber_object_gc`: everything a fiber owns — the resolved call of
/// a fiber not started yet, the callable, the frames of a suspended fiber,
/// the value in transit and the return value. (Rust temporaries on the
/// fiber's native stack stay invisible: treated as external references.)
fn gc_traverse(d: &FiberData, v: &mut crate::gc::GcVisitor<'_>) {
    if let Some(c) = &d.call {
        v.pending_call(c);
    }
    v.value(&d.callable);
    for f in &d.frames {
        v.frame(f);
    }
    if let Some(Ok(x)) = &d.transfer {
        v.value(x);
    }
    v.value(&d.ret);
    for o in d.nested.iter().chain(&d.target) {
        v.obj(o);
    }
}

pub(crate) fn register(vm: &mut Vm) {
    crate::gc::register_traverse_with::<FiberData>(gc_traverse);
    ClassBuilder::new("Fiber")
        .flags(cflags::FINAL | cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .method(
            "__construct(callable $callback)",
            flags::PUBLIC,
            fiber_construct,
        )
        .method("start(mixed ...$args): mixed", flags::PUBLIC, fiber_start)
        .method(
            "resume(mixed $value = null): mixed",
            flags::PUBLIC,
            fiber_resume,
        )
        .method(
            "throw(Throwable $exception): mixed",
            flags::PUBLIC,
            fiber_throw,
        )
        .method("getReturn(): mixed", flags::PUBLIC, fiber_get_return)
        .method("isStarted(): bool", flags::PUBLIC, fiber_is_started)
        .method("isSuspended(): bool", flags::PUBLIC, fiber_is_suspended)
        .method("isRunning(): bool", flags::PUBLIC, fiber_is_running)
        .method("isTerminated(): bool", flags::PUBLIC, fiber_is_terminated)
        .method(
            "suspend(mixed $value = null): mixed",
            flags::PUBLIC | flags::STATIC,
            fiber_suspend,
        )
        .method(
            "getCurrent(): ?Fiber",
            flags::PUBLIC | flags::STATIC,
            fiber_get_current,
        )
        // The `dtor_obj` handler. php-src has no such method; ClassBuilder
        // only wires destructors through `__destruct`.
        .method("__destruct(): void", flags::PUBLIC, fiber_destroy)
        .register(vm);
    ClassBuilder::new("FiberError")
        .extends(vm, "Error")
        .flags(cflags::FINAL)
        .method("__construct()", flags::PUBLIC, fiber_error_construct)
        .register(vm);
    // The graceful-exit unwinder: a hidden class outside the Throwable
    // hierarchy, so no `catch` matches it.
    let cls = ClassBuilder::new("GracefulExit")
        .flags(cflags::FINAL)
        .register(vm);
    vm.classes.remove(&cls.lc_name);
    vm.class_order.retain(|n| *n != cls.lc_name);
    GRACEFUL_EXIT.with(|g| *g.borrow_mut() = Some(cls));
}

/// The callable a fiber was constructed with (`None` for a non-fiber).
pub(crate) fn fiber_callable(o: &Obj) -> Option<Value> {
    object::internal::<FiberData, _>(o, |d| d.callable.clone())
}

// --- `_ZendTestFiber` (`ext/zend_test/fiber.c`) ---------------------------

fn test_fiber_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    let callable = a.at(0);
    let call = resolve_fiber_callable(vm, a, &callable)?;
    object::set_internal(&o, FiberData::new(call, callable, FiberKind::Test));
    Ok(Value::Null)
}

/// `delegate_transfer_result`: a fiber that terminated returns `NULL`
/// from the call that resumed it; then its `pipeTo()` target starts with
/// its return value, in the caller's context.
fn test_fiber_delegate(vm: &mut Vm, fiber: &Obj, r: VmResult<Value>) -> VmResult<Value> {
    let v = r?;
    if state_of(fiber) != FiberState::Dead {
        return Ok(v);
    }
    let Some((target, ret)) = with(fiber, |d| {
        let threw = d.threw || d.bailout;
        d.target
            .take()
            .filter(|_| !threw)
            .map(|t| (t, d.ret.clone()))
    })
    .flatten() else {
        return Ok(Value::Null);
    };
    vm.fiber_create_context(&target, vec![ret], Vec::new())?;
    let r = vm.fiber_switch_in(&target, Ok(Value::Null));
    test_fiber_delegate(vm, &target, r).map(|_| Value::Null)
}

fn test_fiber_start(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    if with(&o, |d| d.is_target).unwrap_or(false) {
        return Err(vm.make_throw(
            "Error",
            "Cannot start a fiber that is the target of another fiber",
        ));
    }
    if state_of(&o) != FiberState::Init {
        return Err(vm.fiber_error("Cannot start a fiber that has already been started"));
    }
    let args: Vec<Value> = a.values.clone();
    let named = std::mem::take(&mut a.named);
    vm.fiber_create_context(&o, args, named)?;
    let r = vm.fiber_switch_in(&o, Ok(Value::Null));
    test_fiber_delegate(vm, &o, r)
}

fn test_fiber_resume(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    if state_of(&o) != FiberState::Suspended {
        return Err(vm.make_throw("Error", "Cannot resume a fiber that is not suspended"));
    }
    let v = a.get(0).unwrap_or(Value::Null);
    let r = vm.fiber_switch_in(&o, Ok(v));
    test_fiber_delegate(vm, &o, r)
}

fn test_fiber_pipe_to(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    let callable = a.at(0);
    let call = resolve_fiber_callable(vm, a, &callable)?;
    let target = object::new_object(object::class_of(&o).clone(), Vec::new());
    let mut data = FiberData::new(call, callable, FiberKind::Test);
    data.is_target = true;
    object::set_internal(&target, data);
    with(&o, |d| d.target = Some(target.clone()));
    Ok(Value::Object(target))
}

fn test_fiber_suspend(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.get(0).unwrap_or(Value::Null);
    suspend_innermost(vm, v, FiberKind::Test)
}

/// Registers `zend_test`'s `_ZendTestFiber` (`zend_test_fiber_init`): a
/// fiber class with its own active-fiber pointer, so that it and `Fiber`
/// can suspend each other's callers.
pub fn register_zend_test_fiber(vm: &mut Vm) {
    ClassBuilder::new("_ZendTestFiber")
        .flags(cflags::FINAL | cflags::NO_CLONE)
        .method(
            "__construct(callable $callback)",
            flags::PUBLIC,
            test_fiber_construct,
        )
        .method(
            "start(mixed ...$args): mixed",
            flags::PUBLIC,
            test_fiber_start,
        )
        .method(
            "resume(mixed $value = null): mixed",
            flags::PUBLIC,
            test_fiber_resume,
        )
        .method(
            "pipeTo(callable $callback): _ZendTestFiber",
            flags::PUBLIC,
            test_fiber_pipe_to,
        )
        .method(
            "suspend(mixed $value = null): mixed",
            flags::PUBLIC | flags::STATIC,
            test_fiber_suspend,
        )
        // The `dtor_obj` handler (`zend_test_fiber_object_destroy`).
        .method("__destruct(): void", flags::PUBLIC, fiber_destroy)
        .register(vm);
}
