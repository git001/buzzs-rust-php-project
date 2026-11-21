// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Stream and stream-context resources.
//!
//! A stream resource's payload shares a [`StreamCell`] (the library
//! [`Stream`] plus VM-side data). A registry maps library stream ids to
//! cells so values the library hands back (`$this->stream` of user filters,
//! streams returned by user wrappers) can be turned into resources again.
//! When the last reference goes away the stream is closed; if closing needs
//! PHP code (user-space wrapper or filter) and no native call is running,
//! the stream is queued and closed by the next stream function or at
//! request shutdown.

use std::cell::{Cell, RefCell, RefMut};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use php_runtime::{Resource, Value, ZArray};
use php_streams::context::{ContextRef, StreamContext};
use php_streams::stream::Stream;
use php_vm::ext::{Args, VmResult};
use php_vm::Vm;

use crate::bridge;

/// Resource type name of streams.
pub const STREAM: &str = "stream";
/// Resource type name of persistent streams.
pub const PERSISTENT_STREAM: &str = "persistent stream";
/// Resource type name of stream contexts.
pub const CONTEXT: &str = "stream-context";

/// The shared state behind a stream resource.
#[derive(Debug)]
pub struct StreamCell {
    stream: RefCell<Option<Stream>>,
    id: Cell<u64>,
    handle: Cell<i64>,
    /// The user-space wrapper object (`wrapper_data` in the meta data).
    pub wrapper_data: RefCell<Option<Value>>,
    /// Opened with `pfsockopen()`.
    pub persistent: bool,
    /// The context resource state attached to the stream (kept alive like
    /// PHP's `stream->ctx` reference).
    ctx_keep: RefCell<Option<Rc<CtxCell>>>,
    /// `fclose()` was called while the stream was in use: close it when
    /// the running operation ends.
    close_requested: Cell<bool>,
    /// A stand-in for a stream that has no resource of its own (opened by
    /// `file_get_contents()`, `include`, ...) while its user filter runs:
    /// never holds the stream, which is busy.
    proxy: bool,
}

impl StreamCell {
    /// Borrows the stream, `None` when closed or already in use (a user
    /// callback operating on the stream it was called for).
    pub fn get(&self) -> Option<RefMut<'_, Stream>> {
        let b = self.stream.try_borrow_mut().ok()?;
        RefMut::filter_map(b, Option::as_mut).ok()
    }

    /// Whether the stream has been closed.
    pub fn is_closed(&self) -> bool {
        !self.proxy && self.stream.try_borrow().is_ok_and(|s| s.is_none())
    }

    /// Removes the stream (for closing).
    pub fn take(&self) -> Option<Stream> {
        self.stream.try_borrow_mut().ok()?.take()
    }

    /// Asks for the stream to be closed when its running operation ends.
    pub fn request_close(&self) {
        self.close_requested.set(true);
    }

    /// The library stream id.
    pub fn id(&self) -> u64 {
        self.id.get()
    }

    /// Replaces the stream behind the resource (e.g. a TLS upgrade of a
    /// socket), keeping the resource identity. Returns the old stream.
    pub fn replace_stream(self: &Rc<Self>, new: Stream) -> Option<Stream> {
        let new_id = new.id();
        let old = self.stream.try_borrow_mut().ok()?.replace(new);
        STREAMS.with(|m| {
            let mut m = m.borrow_mut();
            m.remove(&self.id.get());
            m.insert(new_id, Rc::downgrade(self));
        });
        self.id.set(new_id);
        old
    }

    /// The resource handle number.
    pub fn handle(&self) -> i64 {
        self.handle.get()
    }

    /// Keeps a context resource alive with the stream.
    pub fn keep_context(&self, v: Value) {
        if let Value::Resource(r) = v {
            *self.ctx_keep.borrow_mut() = ctx_cell_of(&r);
        }
    }
}

/// Whether closing `s` runs PHP code.
/// Whether `s` was opened through a user-space wrapper (file or directory).
pub fn is_user_stream(s: &Stream) -> bool {
    s.ops().is_userspace()
        || s.wrapper
            .as_ref()
            .is_some_and(|w| w.label == php_streams::user::LABEL)
}

fn needs_vm(s: &Stream) -> bool {
    is_user_stream(s)
        || s.read_filters
            .filters
            .iter()
            .chain(s.write_filters.filters.iter())
            .any(|f| f.filter.label() == "user-filter")
}

impl Drop for StreamCell {
    fn drop(&mut self) {
        if self.proxy {
            return;
        }
        let _ = STREAMS.try_with(|m| {
            if let Ok(mut m) = m.try_borrow_mut() {
                m.remove(&self.id.get());
            }
        });
        let Some(mut s) = self.stream.get_mut().take() else {
            return;
        };
        // The resource is gone: user filters see `$this->stream === null`.
        let id = s.id();
        let _ = RELEASED.try_with(|r| r.borrow_mut().insert(id));
        if needs_vm(&s) && !bridge::has_vm() {
            // Released by the executor itself (a CV going out of scope):
            // the close runs PHP code, so it waits for the next safe point
            // (`zend_list_free` closes at once in php-src).
            let _ = DEFERRED.try_with(|d| d.borrow_mut().push(s));
            php_vm::interrupt::request_interrupt();
        } else {
            s.close();
            let _ = RELEASED.try_with(|r| r.borrow_mut().remove(&id));
        }
    }
}

/// The resource payload of a stream.
#[derive(Debug)]
pub struct StreamRes(pub Rc<StreamCell>);

/// The resource payload of the memory stream a `php://temp` or `data:`
/// stream encloses (`php_stream_temp_create()` registers it as a resource
/// of its own; `php_stream_encloses()` sets `PHP_STREAM_FLAG_NO_FCLOSE` on
/// it). It stands for the enclosing stream, which reads and writes through
/// it.
#[derive(Debug)]
pub struct EnclosedRes(pub Weak<StreamCell>);

/// The shared state behind a stream-context resource.
#[derive(Debug)]
pub struct CtxCell {
    /// The library context.
    pub ctx: ContextRef,
    /// `params["notification"]`.
    pub notifier: RefCell<Option<Value>>,
    /// The options as PHP values (what `stream_context_get_options()`
    /// returns).
    pub options: RefCell<ZArray>,
    handle: i64,
}

impl Drop for CtxCell {
    fn drop(&mut self) {
        // Forget the registry entry of a released context: its `Weak`
        // would keep the cell's allocation alive (`zend_list_free` frees
        // the context with its resource, so `memory_get_usage()` does not
        // grow with every context created; bug #75306).
        let key = Rc::as_ptr(&self.ctx) as usize;
        let _ = CONTEXTS.try_with(|m| {
            if let Ok(mut m) = m.try_borrow_mut() {
                if m.get(&key).is_some_and(|w| w.strong_count() == 0) {
                    m.remove(&key);
                }
            }
        });
    }
}

/// The resource payload of a stream context.
#[derive(Debug)]
pub struct CtxRes(pub Rc<CtxCell>);

thread_local! {
    static STREAMS: RefCell<HashMap<u64, Weak<StreamCell>>> = RefCell::new(HashMap::new());
    static CONTEXTS: RefCell<HashMap<usize, Weak<CtxCell>>> = RefCell::new(HashMap::new());
    static DEFERRED: RefCell<Vec<Stream>> = const { RefCell::new(Vec::new()) };
    /// Streams a fatal error left open (see [`park_bailed`]).
    static BAILED: RefCell<Vec<Stream>> = const { RefCell::new(Vec::new()) };
    /// Streams being closed by `fclose()` (their resource is still valid).
    static FCLOSING: RefCell<std::collections::HashSet<u64>> =
        RefCell::new(std::collections::HashSet::new());
    /// Streams whose resource was released while they still close.
    static RELEASED: RefCell<std::collections::HashSet<u64>> =
        RefCell::new(std::collections::HashSet::new());
    /// Persistent ids of open persistent streams.
    static PERSISTENT: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
    static DEFAULT_CONTEXT: RefCell<Option<Resource>> = const { RefCell::new(None) };
    /// The enclosed-stream resources ([`EnclosedRes`]) by the library id of
    /// their enclosing stream.
    static ENCLOSED: RefCell<HashMap<u64, Resource>> = RefCell::new(HashMap::new());
    /// [`ENCLOSED`] size at which entries of closed streams are pruned.
    static ENCLOSED_PRUNE_AT: Cell<usize> = const { Cell::new(16) };
}

/// Whether `php_stream_temp_create()` backs a stream of this type with an
/// enclosed memory stream resource.
fn has_enclosed_stream(s: &Stream) -> bool {
    matches!(s.ops_label(), "TEMP" | "RFC2397")
}

/// Registers the enclosed-stream resource of `cell`, numbered right after
/// the enclosing stream's (`php_stream_memory_create()` inside
/// `php_stream_temp_create()`).
fn register_enclosed(cell: &Rc<StreamCell>) {
    let inner = Resource::new(STREAM, Some(Box::new(EnclosedRes(Rc::downgrade(cell)))));
    ENCLOSED.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= ENCLOSED_PRUNE_AT.with(Cell::get) {
            // Streams released without fclose(): php_stream_free_enclosed().
            m.retain(|_, r| {
                let live = enclosing_cell(r).is_some_and(|c| !c.is_closed());
                if !live {
                    r.close();
                }
                live
            });
            ENCLOSED_PRUNE_AT.with(|p| p.set((m.len() * 2).max(16)));
        }
        m.insert(cell.id(), inner);
    });
}

/// The enclosing stream of an enclosed-stream resource.
fn enclosing_cell(r: &Resource) -> Option<Rc<StreamCell>> {
    r.data()
        .as_ref()
        .and_then(|d| d.downcast_ref::<EnclosedRes>())
        .and_then(|e| e.0.upgrade())
}

/// Whether `r` is the enclosed memory stream of a `php://temp` or `data:`
/// stream (`PHP_STREAM_FLAG_NO_FCLOSE`: `fclose()` refuses it).
pub fn is_enclosed(r: &Resource) -> bool {
    r.data()
        .as_ref()
        .is_some_and(|d| d.downcast_ref::<EnclosedRes>().is_some())
}

/// `php_stream_free_enclosed()`: the enclosing stream `id` was closed, so
/// its enclosed-stream resource is freed too.
pub fn release_enclosed(id: u64) {
    if let Some(r) = ENCLOSED.with(|m| m.borrow_mut().remove(&id)) {
        r.close();
    }
}

/// Wraps a library stream in a new resource.
pub fn new_stream(stream: Stream, wrapper_data: Option<Value>) -> Value {
    new_stream_ex(stream, wrapper_data, false)
}

/// Request shutdown: `FG(default_context)` goes with the request's
/// resources (`php_shutdown_file_globals`); the next one starts without.
pub(crate) fn reset_default_context() {
    DEFAULT_CONTEXT.with(|d| *d.borrow_mut() = None);
}

/// Whether `ctx` is `FG(default_context)`.
pub fn is_default_context(ctx: &ContextRef) -> bool {
    DEFAULT_CONTEXT.with(|d| {
        d.borrow()
            .as_ref()
            .and_then(ctx_cell_of)
            .is_some_and(|c| Rc::ptr_eq(&c.ctx, ctx))
    })
}

/// Wraps a library stream in a new (optionally persistent) resource.
pub fn new_stream_ex(mut stream: Stream, wrapper_data: Option<Value>, persistent: bool) -> Value {
    // A stream opened with the default context does not keep it.
    if stream.context.as_ref().is_some_and(is_default_context) {
        stream.context = None;
    }
    let id = stream.id();
    let encloses = has_enclosed_stream(&stream);
    let cell = Rc::new(StreamCell {
        ctx_keep: RefCell::new(stream.context.as_ref().and_then(ctx_cell_of_lib)),
        stream: RefCell::new(Some(stream)),
        id: Cell::new(id),
        handle: Cell::new(0),
        wrapper_data: RefCell::new(wrapper_data),
        persistent,
        proxy: false,
        close_requested: Cell::new(false),
    });
    let ty = if persistent {
        PERSISTENT_STREAM
    } else {
        STREAM
    };
    let r = Resource::new(ty, Some(Box::new(StreamRes(Rc::clone(&cell)))));
    cell.handle.set(r.handle());
    STREAMS.with(|m| m.borrow_mut().insert(id, Rc::downgrade(&cell)));
    if encloses {
        register_enclosed(&cell);
    }
    Value::Resource(r)
}

/// Wraps a socket stream; with a persistent id it becomes a `persistent
/// stream` found again by [`persistent_stream`].
pub fn new_persistent(stream: Stream, pid: Option<String>) -> Value {
    let id = stream.id();
    let v = new_stream_ex(stream, None, pid.is_some());
    if let Some(p) = pid {
        PERSISTENT.with(|m| m.borrow_mut().insert(p, id));
    }
    v
}

/// `php_stream_from_persistent_id()`: the live stream opened with `pid`.
pub fn persistent_stream(pid: &str) -> Option<Value> {
    let id = PERSISTENT.with(|m| m.borrow().get(pid).copied())?;
    let v = stream_value_by_id(id);
    if v.is_none() {
        PERSISTENT.with(|m| m.borrow_mut().remove(pid));
    }
    v
}

/// Wraps a library stream in a resource with a fixed handle (`STDIN` & co.).
pub fn new_stream_with_handle(stream: Stream, handle: i64) -> Value {
    let id = stream.id();
    let cell = Rc::new(StreamCell {
        stream: RefCell::new(Some(stream)),
        id: Cell::new(id),
        handle: Cell::new(handle),
        wrapper_data: RefCell::new(None),
        persistent: false,
        ctx_keep: RefCell::new(None),
        proxy: false,
        close_requested: Cell::new(false),
    });
    let r = Resource::with_handle(handle, STREAM, Some(Box::new(StreamRes(Rc::clone(&cell)))));
    STREAMS.with(|m| m.borrow_mut().insert(id, Rc::downgrade(&cell)));
    Value::Resource(r)
}

/// The cell of a stream resource (open or not). An enclosed-stream
/// resource gives its enclosing stream's.
pub fn cell_of(r: &Resource) -> Option<Rc<StreamCell>> {
    let d = r.data();
    let d = d.as_ref()?;
    if let Some(s) = d.downcast_ref::<StreamRes>() {
        return Some(Rc::clone(&s.0));
    }
    d.downcast_ref::<EnclosedRes>().and_then(|e| e.0.upgrade())
}

/// The library stream id of a stream resource.
pub fn stream_id_of(r: &Resource) -> Option<u64> {
    cell_of(r).map(|c| c.id())
}

/// A resource value for the live stream with library id `id`.
pub fn stream_value_by_id(id: u64) -> Option<Value> {
    let cell = STREAMS.with(|m| m.borrow().get(&id).and_then(Weak::upgrade))?;
    if cell.is_closed() {
        return None;
    }
    let ty = if cell.persistent {
        PERSISTENT_STREAM
    } else {
        STREAM
    };
    let r = Resource::with_handle(
        cell.handle(),
        ty,
        Some(Box::new(StreamRes(Rc::clone(&cell)))),
    );
    Some(Value::Resource(r))
}

/// `$this->stream` for a user filter of stream `id`: its resource; `null`
/// while the resource is being closed (`res->type < 0`); for a stream
/// without a resource of its own, a stand-in resource that functions see
/// as a busy stream (`feof()` reports the filter-time EOF flag).
pub fn filter_stream_value(id: u64) -> Value {
    if RELEASED.with(|r| r.borrow().contains(&id)) {
        return Value::Null;
    }
    let cell = STREAMS.with(|m| m.borrow().get(&id).and_then(Weak::upgrade));
    match cell {
        // fclose(): the resource is still valid while the stream closes.
        Some(c) if c.is_closed() && FCLOSING.with(|f| f.borrow().contains(&id)) => {
            Value::Resource(Resource::with_handle(
                c.handle(),
                if c.persistent {
                    PERSISTENT_STREAM
                } else {
                    STREAM
                },
                Some(Box::new(StreamRes(Rc::clone(&c)))),
            ))
        }
        Some(c) if c.is_closed() => Value::Null,
        Some(_) => stream_value_by_id(id).unwrap_or(Value::Null),
        None => {
            let cell = Rc::new(StreamCell {
                stream: RefCell::new(None),
                id: Cell::new(id),
                handle: Cell::new(0),
                wrapper_data: RefCell::new(None),
                persistent: false,
                ctx_keep: RefCell::new(None),
                proxy: true,
                close_requested: Cell::new(false),
            });
            let r = Resource::new(STREAM, Some(Box::new(StreamRes(Rc::clone(&cell)))));
            cell.handle.set(r.handle());
            Value::Resource(r)
        }
    }
}

/// Closes streams whose last reference was dropped while no native call
/// was running.
pub fn close_deferred(vm: &mut Vm) {
    loop {
        let pending = DEFERRED.with(|d| std::mem::take(&mut *d.borrow_mut()));
        if pending.is_empty() {
            break;
        }
        for mut s in pending {
            let id = s.id();
            let _ = bridge::run(vm, || s.close());
            RELEASED.with(|r| r.borrow_mut().remove(&id));
        }
    }
}

/// Interrupt handler: closes the streams [`StreamCell`]'s `Drop` deferred
/// right after the opcode that released them.
pub fn close_deferred_on_interrupt(vm: &mut Vm) -> VmResult<()> {
    if DEFERRED.with(|d| !d.borrow().is_empty()) {
        close_deferred(vm);
    }
    Ok(())
}

/// Keeps a stream without a resource of its own (opened by `include`) that
/// a fatal error interrupted open until request shutdown: `zend_bailout()`
/// skips its `php_stream_close()`, so php-src finds it in
/// `EG(regular_list)` there.
pub fn park_bailed(s: Stream) {
    BAILED.with(|b| b.borrow_mut().push(s));
}

/// `zend_close_rsrc_list()` for the [`park_bailed`] streams: newest first;
/// a fatal error raised by one `stream_close()` does not stop the others
/// (each close runs in its own `zend_try`), and streams parked meanwhile
/// (an `include` inside `stream_close()` that bails out again, GH-20286)
/// are closed from the top again.
fn close_bailed(vm: &mut Vm) {
    loop {
        let next = BAILED.with(|b| {
            let mut b = b.borrow_mut();
            let i = b
                .iter()
                .enumerate()
                .max_by_key(|(_, s)| s.id())
                .map(|(i, _)| i)?;
            Some(b.swap_remove(i))
        });
        let Some(mut s) = next else {
            break;
        };
        let _ = bridge::run(vm, || s.close());
    }
}

/// Request shutdown: closes every stream still open, most recent first
/// (`zend_close_rsrc_list`).
pub fn shutdown(vm: &mut Vm) {
    close_deferred(vm);
    close_bailed(vm);
    let mut cells: Vec<Rc<StreamCell>> =
        STREAMS.with(|m| m.borrow().values().filter_map(Weak::upgrade).collect());
    cells.sort_by_key(|c| std::cmp::Reverse(c.handle()));
    for c in cells {
        if c.handle() <= 3 {
            // STDIN/STDOUT/STDERR keep their descriptors (NO_CLOSE), but
            // their write filters are flushed when the resource list is
            // destroyed.
            let filtered = c.get().is_some_and(|s| !s.write_filters.is_empty());
            if filtered {
                if let Some(mut s) = c.take() {
                    if let Err(php_vm::vm::Throw::Exception(e)) = bridge::run(vm, || s.close()) {
                        report_uncaught(vm, &e);
                    }
                }
            }
            continue;
        }
        if let Some(mut s) = c.take() {
            let _ = bridge::run(vm, || s.close());
        }
        close_bailed(vm);
    }
    close_deferred(vm);
    close_bailed(vm);
}

/// Runs `f` (closing stream `id`) as `fclose()` does: user filters still
/// see the stream's resource.
pub fn fclosing<R>(id: u64, f: impl FnOnce() -> R) -> R {
    FCLOSING.with(|s| s.borrow_mut().insert(id));
    let r = f();
    FCLOSING.with(|s| s.borrow_mut().remove(&id));
    r
}

/// An exception escaping a filter callback while the resource list is
/// destroyed: "Fatal error: Uncaught ..." (`zend_exception_error`).
fn report_uncaught(vm: &mut Vm, e: &php_runtime::Obj) {
    let text = match vm.call_method_by_name(e, "__toString", Vec::new()) {
        Ok(Value::Str(s)) => s.to_string_lossy(),
        _ => return,
    };
    let _ = vm.raise(1, &format!("Uncaught {text}\n  thrown"));
}

/// `get_resources()`: the live stream and context resources (the ones this
/// extension owns), by handle.
pub fn live_resources() -> Vec<(i64, &'static str, Value)> {
    let mut out: Vec<(i64, &'static str, Value)> = Vec::new();
    let cells: Vec<Rc<StreamCell>> =
        STREAMS.with(|m| m.borrow().values().filter_map(Weak::upgrade).collect());
    for c in cells {
        if c.is_closed() {
            continue;
        }
        let ty = if c.persistent {
            PERSISTENT_STREAM
        } else {
            STREAM
        };
        let r = Resource::with_handle(c.handle(), ty, Some(Box::new(StreamRes(Rc::clone(&c)))));
        out.push((c.handle(), ty, Value::Resource(r)));
        if let Some(inner) = ENCLOSED.with(|m| m.borrow().get(&c.id()).cloned()) {
            out.push((inner.handle(), STREAM, Value::Resource(inner)));
        }
    }
    let ctxs: Vec<Rc<CtxCell>> =
        CONTEXTS.with(|m| m.borrow().values().filter_map(Weak::upgrade).collect());
    for c in ctxs {
        let h = c.handle;
        let r = Resource::with_handle(h, CONTEXT, Some(Box::new(CtxRes(c))));
        out.push((h, CONTEXT, Value::Resource(r)));
    }
    out.sort_by_key(|(h, _, _)| *h);
    out
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// Creates a context resource.
pub fn new_context(ctx: StreamContext) -> Value {
    let lib = ctx.shared();
    let r = Resource::new(CONTEXT, None);
    let cell = Rc::new(CtxCell {
        ctx: Rc::clone(&lib),
        notifier: RefCell::new(None),
        options: RefCell::new(ZArray::new()),
        handle: r.handle(),
    });
    *r.data_mut() = Some(Box::new(CtxRes(Rc::clone(&cell))));
    CONTEXTS.with(|m| {
        m.borrow_mut()
            .insert(Rc::as_ptr(&lib) as usize, Rc::downgrade(&cell));
    });
    Value::Resource(r)
}

/// The references open streams hold to context resource `r`
/// (`php_stream_context_set()`: `GC_ADDREF(context->res)` for
/// `stream->ctx`), which `debug_zval_dump()` counts (GH-10885). Directory
/// streams do not attach their context.
pub fn context_refs_held(r: &Resource) -> usize {
    let Some(ctx) = ctx_cell_of(r) else {
        return 0;
    };
    let cells: Vec<Rc<StreamCell>> =
        STREAMS.with(|m| m.borrow().values().filter_map(Weak::upgrade).collect());
    let holds = |c: &StreamCell| {
        let keeps = c
            .ctx_keep
            .borrow()
            .as_ref()
            .is_some_and(|k| Rc::ptr_eq(k, &ctx));
        keeps
            && match c.stream.try_borrow() {
                Ok(s) => s
                    .as_ref()
                    .is_some_and(|s| !s.has_flag(php_streams::stream::flags::IS_DIR)),
                // In use by a running operation: still open.
                Err(_) => true,
            }
    };
    cells.iter().filter(|c| holds(c)).count()
}

/// The cell of a context resource.
pub fn ctx_cell_of(r: &Resource) -> Option<Rc<CtxCell>> {
    if r.is_closed() {
        return None;
    }
    r.data()
        .as_ref()
        .and_then(|d| d.downcast_ref::<CtxRes>())
        .map(|s| Rc::clone(&s.0))
}

/// A resource value for a library context (user wrappers' `$context`).
pub fn context_value(ctx: &ContextRef) -> Value {
    let cell = CONTEXTS.with(|m| {
        m.borrow()
            .get(&(Rc::as_ptr(ctx) as usize))
            .and_then(Weak::upgrade)
    });
    match cell {
        Some(c) => {
            let handle = c.handle;
            Value::Resource(Resource::with_handle(
                handle,
                CONTEXT,
                Some(Box::new(CtxRes(c))),
            ))
        }
        None => Value::Null,
    }
}

/// The context cell of a library context.
pub fn ctx_cell_of_lib(ctx: &ContextRef) -> Option<Rc<CtxCell>> {
    CONTEXTS.with(|m| {
        m.borrow()
            .get(&(Rc::as_ptr(ctx) as usize))
            .and_then(Weak::upgrade)
    })
}

/// `FG(default_context)` if it has been created.
pub fn existing_default_context() -> Option<ContextRef> {
    DEFAULT_CONTEXT.with(|d| {
        d.borrow()
            .as_ref()
            .and_then(ctx_cell_of)
            .map(|c| Rc::clone(&c.ctx))
    })
}

/// `FG(default_context)`, created on first use.
pub fn default_context() -> Resource {
    DEFAULT_CONTEXT.with(|d| {
        let mut d = d.borrow_mut();
        if let Some(r) = d.as_ref() {
            return r.clone();
        }
        let Value::Resource(r) = new_context(StreamContext::new()) else {
            unreachable!("new_context returns a resource")
        };
        *d = Some(r.clone());
        r
    })
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// The running function's name (for messages).
pub fn fname(a: &Args) -> String {
    a.func
        .as_ref()
        .map_or_else(String::new, |f| f.name.to_string_lossy())
}

/// Argument `n` (0-based) as a resource, with ZPP's `TypeError`.
pub fn resource_arg(vm: &mut Vm, a: &Args, n: usize) -> VmResult<Resource> {
    match a.at(n) {
        Value::Resource(r) => Ok(r),
        other => Err(a.type_error(
            vm,
            n + 1,
            &format!(
                "must be of type resource, {} given",
                bridge::given_type(&other)
            ),
        )),
    }
}

/// `PHP_Z_PARAM_STREAM`: argument `n` as an open stream.
pub fn stream_arg(vm: &mut Vm, a: &Args, n: usize) -> VmResult<Rc<StreamCell>> {
    let r = resource_arg(vm, a, n)?;
    match cell_of(&r) {
        Some(c) if !r.is_closed() && !c.is_closed() => Ok(c),
        _ => Err(a.type_error(vm, n + 1, "must be an open stream resource")),
    }
}

/// `PHP_Z_PARAM_STREAM` returning the resource too.
pub fn stream_arg_res(vm: &mut Vm, a: &Args, n: usize) -> VmResult<(Resource, Rc<StreamCell>)> {
    let r = resource_arg(vm, a, n)?;
    match cell_of(&r) {
        Some(c) if !r.is_closed() && !c.is_closed() => Ok((r, c)),
        _ => Err(a.type_error(vm, n + 1, "must be an open stream resource")),
    }
}

/// The open stream behind a resource, or the "not a valid stream
/// resource" `TypeError`.
pub fn stream_of_resource(vm: &mut Vm, a: &Args, r: &Resource) -> VmResult<Rc<StreamCell>> {
    match cell_of(r) {
        Some(c) if !r.is_closed() && !c.is_closed() => Ok(c),
        _ => Err(vm.throw(
            "TypeError",
            &format!(
                "{}(): supplied resource is not a valid stream resource",
                fname(a)
            ),
        )),
    }
}

/// `php_stream_context_from_zval`: argument `n` as a context; `null` gives
/// the default context unless `no_default`.
pub fn context_arg(
    vm: &mut Vm,
    a: &Args,
    n: usize,
    no_default: bool,
) -> VmResult<Option<ContextRef>> {
    match a.at(n) {
        Value::Null => {
            if no_default {
                crate::errors::set_operation_context(None);
                Ok(None)
            } else {
                let c = ctx_cell_of(&default_context()).map(|c| Rc::clone(&c.ctx));
                crate::errors::set_operation_context(c.clone());
                Ok(c)
            }
        }
        Value::Resource(r) => match ctx_cell_of(&r) {
            Some(c) => {
                crate::errors::set_operation_context(Some(Rc::clone(&c.ctx)));
                Ok(Some(Rc::clone(&c.ctx)))
            }
            None => Err(vm.throw(
                "TypeError",
                &format!(
                    "{}(): supplied resource is not a valid Stream-Context resource",
                    fname(a)
                ),
            )),
        },
        other => Err(a.type_error(
            vm,
            n + 1,
            &format!(
                "must be of type resource or null, {} given",
                bridge::given_type(&other)
            ),
        )),
    }
}

/// A `string` argument as bytes.
pub fn str_arg(a: &Args, n: usize) -> Vec<u8> {
    a.str(n).as_bytes().to_vec()
}

/// A path argument (`Z_PARAM_PATH`): rejects embedded NUL bytes.
pub fn path_arg(vm: &mut Vm, a: &Args, n: usize) -> VmResult<Vec<u8>> {
    let s = str_arg(a, n);
    if s.contains(&0) {
        return Err(a.value_error(vm, n + 1, "must not contain any null bytes"));
    }
    Ok(s)
}

/// Runs `f` on the open stream of `cell` inside a library scope. Returns
/// `None` when the stream is busy (re-entered from its own callback).
pub fn with_stream<R>(
    vm: &mut Vm,
    cell: &StreamCell,
    f: impl FnOnce(&mut Stream) -> R,
) -> VmResult<Option<R>> {
    crate::errors::set_operation_context(cell.get().and_then(|s| s.context.clone()));
    let r = bridge::run_stream(vm, || cell.get().map(|mut s| f(&mut s)));
    if cell.close_requested.replace(false) {
        if let Some(mut s) = cell.take() {
            bridge::run(vm, || s.close())?;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_registry() {
        let s = php_streams::memory::memory_create(php_streams::mode::TempMode::Default);
        let id = s.id();
        let v = new_stream(s, None);
        let Value::Resource(r) = &v else { panic!() };
        assert_eq!(stream_id_of(r), Some(id));
        let alias = stream_value_by_id(id).expect("alias");
        let Value::Resource(ar) = &alias else {
            panic!()
        };
        assert_eq!(ar.handle(), r.handle());
        drop(alias);
        drop(v);
        assert!(stream_value_by_id(id).is_none());
    }

    #[test]
    fn released_contexts_leave_the_registry() {
        let before = CONTEXTS.with(|m| m.borrow().len());
        let v = new_context(StreamContext::new());
        let Some(cell) = (match &v {
            Value::Resource(r) => ctx_cell_of(r),
            _ => None,
        }) else {
            panic!("context resource expected")
        };
        let lib = Rc::clone(&cell.ctx);
        assert!(ctx_cell_of_lib(&lib).is_some());
        assert_eq!(CONTEXTS.with(|m| m.borrow().len()), before + 1);
        drop(cell);
        drop(v);
        assert!(ctx_cell_of_lib(&lib).is_none());
        assert_eq!(CONTEXTS.with(|m| m.borrow().len()), before);
    }

    #[test]
    fn temp_streams_enclose_a_resource() {
        let s = php_streams::memory::temp_create(
            php_streams::mode::TempMode::Default,
            php_streams::memory::MAX_MEM,
            None,
        );
        let id = s.id();
        let v = new_stream(s, None);
        let Value::Resource(r) = &v else { panic!() };
        let inner = ENCLOSED
            .with(|m| m.borrow().get(&id).cloned())
            .expect("enclosed");
        assert_eq!(inner.handle(), r.handle() + 1);
        assert!(is_enclosed(&inner));
        assert!(!is_enclosed(r));
        let cell = cell_of(&inner).expect("enclosing cell");
        assert_eq!(cell.id(), id);
        assert!(live_resources()
            .iter()
            .any(|(h, ty, _)| *h == inner.handle() && *ty == STREAM));
        release_enclosed(id);
        assert!(inner.is_closed());

        // php://memory streams enclose nothing.
        let m = php_streams::memory::memory_create(php_streams::mode::TempMode::Default);
        let mid = m.id();
        let _mv = new_stream(m, None);
        assert!(ENCLOSED.with(|e| e.borrow().get(&mid).is_none()));
    }

    #[test]
    fn open_streams_hold_their_context() {
        let cv = new_context(StreamContext::new());
        let Value::Resource(cr) = &cv else { panic!() };
        let ctx = ctx_cell_of(cr).expect("ctx");
        assert_eq!(context_refs_held(cr), 0);
        let mut s = php_streams::memory::memory_create(php_streams::mode::TempMode::Default);
        s.context = Some(Rc::clone(&ctx.ctx));
        let sv = new_stream(s, None);
        let Value::Resource(sr) = &sv else { panic!() };
        assert_eq!(context_refs_held(cr), 1);
        let cell = cell_of(sr).expect("cell");
        drop(cell.take());
        assert_eq!(context_refs_held(cr), 0);
    }

    #[test]
    fn context_alias() {
        let v = new_context(StreamContext::new());
        let Value::Resource(r) = &v else { panic!() };
        let cell = ctx_cell_of(r).expect("ctx");
        let alias = context_value(&cell.ctx);
        let Value::Resource(ar) = &alias else {
            panic!()
        };
        assert_eq!(ar.handle(), r.handle());
    }
}
