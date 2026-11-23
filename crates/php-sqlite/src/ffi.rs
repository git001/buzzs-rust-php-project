// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Safe wrappers over the system `libsqlite3` C API.
//!
//! This is the only module of the crate containing `unsafe` code. Everything
//! above it works with the owned, reference-counted handles defined here:
//!
//! * [`Connection`] — a `sqlite3*` (closed with `sqlite3_close_v2()` on drop,
//!   or explicitly with `sqlite3_close()`);
//! * [`RawStatement`] — a `sqlite3_stmt*` that keeps its connection alive;
//! * [`RawBlob`] — a `sqlite3_blob*` (incremental BLOB I/O).
//!
//! User-defined functions, aggregates, collations and the authorizer are
//! registered as Rust trait objects ([`ScalarFunction`],
//! [`AggregateFunction`], [`CollationFunction`], [`AuthorizerFunction`]);
//! the `extern "C"` trampolines below convert the C arguments into owned Rust
//! values and dispatch to them.
//!
//! Callbacks need the PHP host (to call user callables). `SQLite` only invokes
//! them synchronously from inside `sqlite3_prepare/step/exec`, so every such
//! call is wrapped in [`enter`], which publishes the current call context
//! ([`Cx`]) in a thread-local stack for the duration of the call; handlers
//! fetch it back with [`with_scope`].

use crate::host::Cx;
use libsqlite3_sys as sys;
use php_runtime::Value;
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::rc::Rc;

pub(crate) use sys::{
    SQLITE_BLOB, SQLITE_BUSY, SQLITE_CONSTRAINT, SQLITE_DONE, SQLITE_ERROR, SQLITE_FLOAT,
    SQLITE_INTEGER, SQLITE_INTERRUPT, SQLITE_LOCKED, SQLITE_MISUSE, SQLITE_NOLFS, SQLITE_NOMEM,
    SQLITE_NOTFOUND, SQLITE_NULL, SQLITE_OK, SQLITE_OPEN_URI, SQLITE_ROW, SQLITE_TEXT,
    SQLITE_TOOBIG, SQLITE_UTF8,
};

/// `SQLITE_ATTACH` authorizer action code.
pub(crate) const SQLITE_ATTACH: i32 = sys::SQLITE_ATTACH;
/// `SQLITE_DENY` authorizer result.
pub(crate) const SQLITE_DENY: i32 = sys::SQLITE_DENY;
/// `SQLITE_IGNORE` authorizer result.
pub(crate) const SQLITE_IGNORE: i32 = sys::SQLITE_IGNORE;

extern "C" {
    // Added in SQLite 3.43; not part of the pre-generated 3.34 bindings.
    fn sqlite3_stmt_explain(stmt: *mut sys::sqlite3_stmt, mode: c_int) -> c_int;
    // Not part of the pre-generated bindings either (available since 3.7.14).
    fn sqlite3_close_v2(db: *mut sys::sqlite3) -> c_int;
}

thread_local! {
    /// The stack of active call contexts (innermost last).
    static SCOPES: RefCell<Vec<*mut Cx<'static>>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` (which performs `SQLite` calls that may invoke user callbacks)
/// with `cx` published as the current call context.
pub(crate) fn enter<R>(cx: &mut Cx<'_>, f: impl FnOnce() -> R) -> R {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            SCOPES.with(|s| {
                s.borrow_mut().pop();
            });
        }
    }
    let raw: *mut Cx<'_> = cx;
    // Only the lifetime parameter is erased; the pointer is removed from the
    // stack (by `Pop`) before `cx`'s borrow ends.
    let raw = raw.cast::<Cx<'static>>();
    SCOPES.with(|s| s.borrow_mut().push(raw));
    let _pop = Pop;
    f()
}

/// Gives a callback handler access to the innermost call context (`None` when
/// `SQLite` invokes a callback outside any [`enter`], e.g. from a destructor).
pub(crate) fn with_scope<R>(f: impl for<'s, 'h> FnOnce(Option<&'s mut Cx<'h>>) -> R) -> R {
    let top = SCOPES.with(|s| s.borrow().last().copied());
    match top {
        // SAFETY: the pointer was pushed by `enter` from a live `&mut Cx`
        // whose borrow outlives the `enter` call, and it is popped before
        // `enter` returns. `enter`'s closure never touches that `Cx` itself
        // (it is mutably borrowed by `enter`), so this is the only live
        // access path while SQLite runs the callback. Nested `enter` calls
        // push reborrows derived from this reference and pop them before
        // control returns here.
        Some(raw) => f(Some(unsafe { &mut *raw })),
        None => f(None),
    }
}

/// Builds a C string from PHP bytes, truncating at the first NUL byte like
/// C code receiving a `char *` does.
fn c_string(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// Copies a possibly-NULL C string.
fn opt_bytes(p: *const c_char) -> Option<Vec<u8>> {
    if p.is_null() {
        None
    } else {
        // SAFETY: SQLite returns either NULL or a valid NUL-terminated
        // string that stays valid until the next call on the same object;
        // we copy it immediately.
        Some(unsafe { CStr::from_ptr(p) }.to_bytes().to_vec())
    }
}

/// Copies `len` bytes at `p` (NULL or zero length gives an empty vector).
fn bytes_at(p: *const c_void, len: c_int) -> Vec<u8> {
    let Ok(len) = usize::try_from(len) else {
        return Vec::new();
    };
    if p.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: SQLite guarantees `p` points to at least `len` bytes (the
    // value of `sqlite3_*_bytes()` called right after the pointer was
    // obtained); the slice is copied before any other SQLite call.
    unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) }.to_vec()
}

/// Length of a byte buffer as a C `int`, saturating (`SQLite` rejects lengths
/// above its limits with `SQLITE_TOOBIG` anyway).
fn c_len(len: usize) -> c_int {
    c_int::try_from(len).unwrap_or(c_int::MAX)
}

/// `sqlite3_libversion()`.
pub(crate) fn libversion() -> String {
    // SAFETY: returns a pointer to a static NUL-terminated string.
    let p = unsafe { sys::sqlite3_libversion() };
    opt_bytes(p).map_or_else(String::new, |b| String::from_utf8_lossy(&b).into_owned())
}

/// `sqlite3_libversion_number()`.
pub(crate) fn libversion_number() -> i32 {
    // SAFETY: no preconditions.
    unsafe { sys::sqlite3_libversion_number() }
}

/// `sqlite3_errstr(rc)`.
pub(crate) fn errstr(rc: i32) -> Vec<u8> {
    // SAFETY: returns a static string for any code.
    opt_bytes(unsafe { sys::sqlite3_errstr(rc) }).unwrap_or_default()
}

/// `sqlite3_errmsg(NULL)` ("out of memory").
pub(crate) fn errmsg_null() -> Vec<u8> {
    // SAFETY: sqlite3_errmsg accepts a NULL handle.
    opt_bytes(unsafe { sys::sqlite3_errmsg(ptr::null_mut()) }).unwrap_or_default()
}

/// `sqlite3_errcode(NULL)`.
pub(crate) fn errcode_null() -> i32 {
    // SAFETY: sqlite3_errcode accepts a NULL handle.
    unsafe { sys::sqlite3_errcode(ptr::null_mut()) }
}

/// `xMalloc` of sqlite's allocator ([`ensure_init`]).
unsafe extern "C" fn mem_malloc(n: c_int) -> *mut c_void {
    let Ok(n) = usize::try_from(n) else {
        return ptr::null_mut();
    };
    // SAFETY: plain allocation; sqlite handles NULL as SQLITE_NOMEM.
    unsafe { php_memlimit::c_heap::malloc(n) }
}

/// `xFree` of sqlite's allocator.
unsafe extern "C" fn mem_free(p: *mut c_void) {
    // SAFETY: sqlite frees only blocks from `mem_malloc`/`mem_realloc`.
    unsafe { php_memlimit::c_heap::free(p) }
}

/// `xRealloc` of sqlite's allocator.
unsafe extern "C" fn mem_realloc(p: *mut c_void, n: c_int) -> *mut c_void {
    let Ok(n) = usize::try_from(n) else {
        return ptr::null_mut();
    };
    // SAFETY: `p` is a live block of these methods.
    unsafe { php_memlimit::c_heap::realloc(p, n) }
}

/// `xSize` of sqlite's allocator.
unsafe extern "C" fn mem_size(p: *mut c_void) -> c_int {
    // SAFETY: sqlite asks only for blocks of these methods.
    c_int::try_from(unsafe { php_memlimit::c_heap::usable(p) }).unwrap_or(c_int::MAX)
}

/// `xRoundup` of sqlite's allocator: malloc rounds by itself.
unsafe extern "C" fn mem_roundup(n: c_int) -> c_int {
    n
}

/// `xInit`/`xShutdown` of sqlite's allocator: nothing to set up.
unsafe extern "C" fn mem_noop(_: *mut c_void) -> c_int {
    sys::SQLITE_OK
}

unsafe extern "C" fn mem_shutdown(_: *mut c_void) {}

/// Puts sqlite's allocator on php-memlimit's C hooks and initialises
/// sqlite, once per process: what a request makes sqlite allocate counts
/// against its `memory_limit`, and past it sqlite gets NULL
/// (`SQLITE_NOMEM`). php-src leaves sqlite on its default allocator; see
/// DIFFERENCES.md. Runs before any call that allocates (`sqlite3_config` must precede
/// `sqlite3_initialize`, which the first such call runs implicitly). What
/// initialisation keeps for the process is persistent.
pub(crate) fn ensure_init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let methods = sys::sqlite3_mem_methods {
            xMalloc: Some(mem_malloc),
            xFree: Some(mem_free),
            xRealloc: Some(mem_realloc),
            xSize: Some(mem_size),
            xRoundup: Some(mem_roundup),
            xInit: Some(mem_noop),
            xShutdown: Some(mem_shutdown),
            pAppData: ptr::null_mut(),
        };
        php_memlimit::persistent(|| {
            // SAFETY: called once, before sqlite is initialised by this
            // crate; sqlite copies the methods (SQLITE_CONFIG_MALLOC), whose
            // functions live for the process. Should another library have initialised sqlite
            // already, sqlite3_config fails with SQLITE_MISUSE and sqlite
            // keeps malloc, which the hooks' design tolerates.
            unsafe {
                sys::sqlite3_config(sys::SQLITE_CONFIG_MALLOC, &raw const methods);
                sys::sqlite3_initialize();
            }
        });
    });
}

/// `sqlite3_mprintf("%q", s)`: doubles single quotes (stops at NUL).
pub(crate) fn escape_q(s: &[u8]) -> Option<Vec<u8>> {
    ensure_init();
    let arg = c_string(s);
    // SAFETY: the format string and argument are valid NUL-terminated
    // strings; the result is freed with sqlite3_free after copying.
    unsafe {
        let p = sys::sqlite3_mprintf(c"%q".as_ptr(), arg.as_ptr());
        if p.is_null() {
            return None;
        }
        let out = CStr::from_ptr(p).to_bytes().to_vec();
        sys::sqlite3_free(p.cast());
        Some(out)
    }
}

/// A value passed to a user-defined function, already converted the way PHP
/// does (`sqlite3_value_text()` for both TEXT and BLOB).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ArgValue {
    /// `SQLITE_INTEGER`.
    Int(i64),
    /// `SQLITE_FLOAT`.
    Float(f64),
    /// `SQLITE_NULL`.
    Null,
    /// `SQLITE_TEXT` / `SQLITE_BLOB`.
    Text(Vec<u8>),
}

/// The result a user-defined function hands back to `SQLite`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FnResult {
    /// `sqlite3_result_int64`.
    Int(i64),
    /// `sqlite3_result_double`.
    Float(f64),
    /// `sqlite3_result_null`.
    Null,
    /// `sqlite3_result_text` (transient copy).
    Text(Vec<u8>),
    /// `sqlite3_result_error(ctx, msg, len)` with an explicit length.
    Error(Vec<u8>, i32),
    /// Leave the result untouched (`SQLite` then yields NULL).
    Unset,
}

/// Per-group state of an aggregate (`php_sqlite3_agg_context` /
/// `aggregate_context`).
#[derive(Debug, Default)]
pub(crate) struct AggregateState {
    /// The PHP context value (`UNDEF` until first used).
    pub context: Option<Value>,
    /// The row counter.
    pub rows: i64,
}

/// A scalar user-defined function.
pub(crate) trait ScalarFunction {
    /// Called for each invocation.
    fn call(&self, args: Vec<ArgValue>) -> FnResult;
}

/// An aggregate user-defined function.
pub(crate) trait AggregateFunction {
    /// `xStep`.
    fn step(&self, state: &mut AggregateState, args: Vec<ArgValue>) -> FnResult;
    /// `xFinal`.
    fn finalize(&self, state: &mut AggregateState) -> FnResult;
}

/// A collation callback.
pub(crate) trait CollationFunction {
    /// Compares two strings.
    fn compare(&self, a: &[u8], b: &[u8]) -> i32;
}

/// The authorizer callback.
pub(crate) trait AuthorizerFunction {
    /// Returns `SQLITE_OK`, `SQLITE_DENY` or `SQLITE_IGNORE`.
    fn authorize(&self, action: i32, args: [Option<&[u8]>; 4]) -> i32;
}

/// The two kinds of user-defined functions.
pub(crate) enum UserFunction {
    /// `xFunc`.
    Scalar(Box<dyn ScalarFunction>),
    /// `xStep` + `xFinal`.
    Aggregate(Box<dyn AggregateFunction>),
}

/// An open database connection.
pub(crate) struct Connection {
    db: Cell<*mut sys::sqlite3>,
    /// Number of `SQLite` calls in progress that may re-enter PHP; closing is
    /// refused while non-zero (closing from inside a callback would free the
    /// handle under the running statement).
    busy: Cell<u32>,
    authorizer: RefCell<Option<Box<Box<dyn AuthorizerFunction>>>>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("open", &!self.db.get().is_null())
            .finish_non_exhaustive()
    }
}

/// The error of a failed [`Connection::open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenError {
    /// The result code of `sqlite3_open_v2()`.
    pub code: i32,
    /// `sqlite3_errmsg()` of the half-open handle, or `sqlite3_errstr()`.
    pub message: Vec<u8>,
}

struct BusyGuard<'a>(&'a Cell<u32>);

impl<'a> BusyGuard<'a> {
    fn new(c: &'a Cell<u32>) -> Self {
        c.set(c.get() + 1);
        BusyGuard(c)
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

impl Connection {
    /// `sqlite3_open_v2(filename, &db, flags, NULL)`.
    pub fn open(filename: &[u8], flags: i32) -> Result<Rc<Connection>, OpenError> {
        ensure_init();
        let name = c_string(filename);
        let mut db: *mut sys::sqlite3 = ptr::null_mut();
        // SAFETY: valid C string and out pointer.
        let rc = unsafe { sys::sqlite3_open_v2(name.as_ptr(), &raw mut db, flags, ptr::null()) };
        if rc != SQLITE_OK {
            let message = if db.is_null() {
                errstr(rc)
            } else {
                // SAFETY: `db` is a (failed) handle that must still be closed.
                let m = opt_bytes(unsafe { sys::sqlite3_errmsg(db) }).unwrap_or_default();
                // SAFETY: closing the handle returned by sqlite3_open_v2.
                unsafe { sys::sqlite3_close(db) };
                m
            };
            return Err(OpenError { code: rc, message });
        }
        Ok(Rc::new(Connection {
            db: Cell::new(db),
            busy: Cell::new(0),
            authorizer: RefCell::new(None),
        }))
    }

    fn raw(&self) -> *mut sys::sqlite3 {
        self.db.get()
    }

    /// `sqlite3_close()` (fails with `SQLITE_BUSY` while statements, blobs
    /// or backups are unfinalized). On success the handle becomes closed.
    pub fn close(&self) -> i32 {
        let db = self.raw();
        if db.is_null() {
            return SQLITE_OK;
        }
        if self.busy.get() > 0 {
            return SQLITE_BUSY;
        }
        // SAFETY: valid open handle not currently executing a callback.
        let rc = unsafe { sys::sqlite3_close(db) };
        if rc == SQLITE_OK {
            self.db.set(ptr::null_mut());
        }
        rc
    }

    /// `sqlite3_errcode()` (or `sqlite3_errcode(NULL)` once closed).
    pub fn errcode(&self) -> i32 {
        // SAFETY: valid handle or NULL (both accepted).
        unsafe { sys::sqlite3_errcode(self.raw()) }
    }

    /// `sqlite3_extended_errcode()`.
    pub fn extended_errcode(&self) -> i32 {
        // SAFETY: valid handle or NULL.
        unsafe { sys::sqlite3_extended_errcode(self.raw()) }
    }

    /// `sqlite3_errmsg()`.
    pub fn errmsg(&self) -> Vec<u8> {
        // SAFETY: valid handle or NULL; the message is copied immediately.
        opt_bytes(unsafe { sys::sqlite3_errmsg(self.raw()) }).unwrap_or_default()
    }

    /// `sqlite3_exec(db, sql, NULL, NULL, &errmsg)`; on failure returns the
    /// result code and the error text (`None` when `SQLite` gave none).
    pub fn exec(&self, sql: &[u8]) -> Result<(), (i32, Option<Vec<u8>>)> {
        let db = self.raw();
        if db.is_null() {
            return Err((SQLITE_MISUSE, None));
        }
        let sql = c_string(sql);
        let mut err: *mut c_char = ptr::null_mut();
        let _busy = BusyGuard::new(&self.busy);
        // SAFETY: valid handle, C string and out pointer; the error string
        // is copied and released with sqlite3_free.
        let rc =
            unsafe { sys::sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), &raw mut err) };
        if rc == SQLITE_OK {
            return Ok(());
        }
        let msg = opt_bytes(err);
        if !err.is_null() {
            // SAFETY: allocated by SQLite.
            unsafe { sys::sqlite3_free(err.cast()) };
        }
        Err((rc, msg))
    }

    /// `sqlite3_prepare_v2(db, sql, len, &stmt, NULL)`. An SQL text without
    /// any statement (empty, whitespace, comments) succeeds with a NULL
    /// statement, exactly like the C API.
    pub fn prepare(self: &Rc<Self>, sql: &[u8]) -> Result<RawStatement, i32> {
        let db = self.raw();
        if db.is_null() {
            return Err(SQLITE_MISUSE);
        }
        let mut stmt: *mut sys::sqlite3_stmt = ptr::null_mut();
        let _busy = BusyGuard::new(&self.busy);
        // SAFETY: `sql` is valid for `len` bytes (SQLite stops at a NUL).
        let rc = unsafe {
            sys::sqlite3_prepare_v2(
                db,
                sql.as_ptr().cast(),
                c_len(sql.len()),
                &raw mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            if !stmt.is_null() {
                // SAFETY: never happens per the API, but finalize defensively.
                unsafe { sys::sqlite3_finalize(stmt) };
            }
            return Err(rc);
        }
        Ok(RawStatement {
            stmt: Cell::new(stmt),
            conn: Rc::clone(self),
            stepping: Cell::new(0),
            finalize_pending: Cell::new(false),
        })
    }

    /// `sqlite3_changes()`.
    pub fn changes(&self) -> i64 {
        if self.raw().is_null() {
            return 0;
        }
        // SAFETY: valid handle.
        i64::from(unsafe { sys::sqlite3_changes(self.raw()) })
    }

    /// `sqlite3_last_insert_rowid()`.
    pub fn last_insert_rowid(&self) -> i64 {
        if self.raw().is_null() {
            return 0;
        }
        // SAFETY: valid handle.
        unsafe { sys::sqlite3_last_insert_rowid(self.raw()) }
    }

    /// `sqlite3_busy_timeout()` (the millisecond count is truncated to a C
    /// `int` like the C extension does implicitly).
    pub fn busy_timeout(&self, ms: i64) -> i32 {
        if self.raw().is_null() {
            return SQLITE_MISUSE;
        }
        #[allow(clippy::cast_possible_truncation)]
        let ms = ms as c_int;
        // SAFETY: valid handle.
        unsafe { sys::sqlite3_busy_timeout(self.raw(), ms) }
    }

    /// `sqlite3_extended_result_codes()`.
    pub fn extended_result_codes(&self, on: i64) -> i32 {
        if self.raw().is_null() {
            return SQLITE_MISUSE;
        }
        #[allow(clippy::cast_possible_truncation)]
        let on = on as c_int;
        // SAFETY: valid handle.
        unsafe { sys::sqlite3_extended_result_codes(self.raw(), on) }
    }

    /// `sqlite3_get_autocommit()`.
    pub fn get_autocommit(&self) -> bool {
        if self.raw().is_null() {
            return true;
        }
        // SAFETY: valid handle.
        unsafe { sys::sqlite3_get_autocommit(self.raw()) != 0 }
    }

    /// `sqlite3_db_config(db, SQLITE_DBCONFIG_DEFENSIVE, on, NULL)`.
    pub fn set_defensive(&self, on: bool) {
        if self.raw().is_null() {
            return;
        }
        // SAFETY: valid handle; this op takes (int, int*) varargs.
        unsafe {
            sys::sqlite3_db_config(
                self.raw(),
                sys::SQLITE_DBCONFIG_DEFENSIVE,
                c_int::from(on),
                ptr::null_mut::<c_int>(),
            );
        }
    }

    /// `sqlite3_load_extension(db, path, 0, &err)`, enabling extension
    /// loading only around the call. `c_api_only` uses
    /// `SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION` (PDO) instead of
    /// `sqlite3_enable_load_extension()` (`SQLite3`).
    pub fn load_extension(&self, path: &[u8], c_api_only: bool) -> Result<(), Option<Vec<u8>>> {
        let db = self.raw();
        if db.is_null() {
            return Err(None);
        }
        let path = c_string(path);
        let toggle = |on: c_int| {
            if c_api_only {
                // SAFETY: valid handle; (int, int*) varargs.
                unsafe {
                    sys::sqlite3_db_config(
                        db,
                        sys::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION,
                        on,
                        ptr::null_mut::<c_int>(),
                    );
                }
            } else {
                // SAFETY: valid handle.
                unsafe { sys::sqlite3_enable_load_extension(db, on) };
            }
        };
        toggle(1);
        let mut err: *mut c_char = ptr::null_mut();
        // SAFETY: valid handle and C strings; the error is freed after copy.
        let rc =
            unsafe { sys::sqlite3_load_extension(db, path.as_ptr(), ptr::null(), &raw mut err) };
        let msg = opt_bytes(err);
        if !err.is_null() {
            // SAFETY: allocated by SQLite.
            unsafe { sys::sqlite3_free(err.cast()) };
        }
        toggle(0);
        if rc == SQLITE_OK {
            Ok(())
        } else {
            Err(msg)
        }
    }

    /// `sqlite3_create_function_v2()`; `None` removes the function.
    pub fn create_function(
        &self,
        name: &[u8],
        nargs: i64,
        flags: i64,
        func: Option<UserFunction>,
    ) -> i32 {
        let db = self.raw();
        if db.is_null() {
            return SQLITE_MISUSE;
        }
        let name = c_string(name);
        #[allow(clippy::cast_possible_truncation)]
        let (nargs, flags) = (nargs as c_int, flags as c_int);
        let Some(func) = func else {
            // SAFETY: valid handle; all callbacks NULL deletes the function.
            return unsafe {
                sys::sqlite3_create_function_v2(
                    db,
                    name.as_ptr(),
                    nargs,
                    flags,
                    ptr::null_mut(),
                    None,
                    None,
                    None,
                    None,
                )
            };
        };
        let is_scalar = matches!(func, UserFunction::Scalar(_));
        let data = Box::into_raw(Box::new(func)).cast::<c_void>();
        // SAFETY: valid handle and name; `data` is a leaked Box released by
        // `destroy_function`, which SQLite calls when the function is
        // replaced, deleted, the connection closes, or this call fails.
        unsafe {
            if is_scalar {
                sys::sqlite3_create_function_v2(
                    db,
                    name.as_ptr(),
                    nargs,
                    flags,
                    data,
                    Some(scalar_trampoline),
                    None,
                    None,
                    Some(destroy_function),
                )
            } else {
                sys::sqlite3_create_function_v2(
                    db,
                    name.as_ptr(),
                    nargs,
                    flags,
                    data,
                    None,
                    Some(step_trampoline),
                    Some(final_trampoline),
                    Some(destroy_function),
                )
            }
        }
    }

    /// `sqlite3_create_collation_v2()`; `None` removes the collation.
    pub fn create_collation(&self, name: &[u8], func: Option<Box<dyn CollationFunction>>) -> i32 {
        let db = self.raw();
        if db.is_null() {
            return SQLITE_MISUSE;
        }
        let name = c_string(name);
        let Some(func) = func else {
            // SAFETY: valid handle; a NULL comparator deletes the collation.
            return unsafe {
                sys::sqlite3_create_collation_v2(
                    db,
                    name.as_ptr(),
                    SQLITE_UTF8,
                    ptr::null_mut(),
                    None,
                    None,
                )
            };
        };
        let data = Box::into_raw(Box::new(func));
        // SAFETY: valid handle and name; `data` is a leaked Box released by
        // `destroy_collation` when SQLite drops the collation.
        let rc = unsafe {
            sys::sqlite3_create_collation_v2(
                db,
                name.as_ptr(),
                SQLITE_UTF8,
                data.cast(),
                Some(collation_trampoline),
                Some(destroy_collation),
            )
        };
        if rc != SQLITE_OK {
            // SAFETY: on failure SQLite does not call xDestroy (documented
            // exception for this interface), so we reclaim the Box.
            drop(unsafe { Box::from_raw(data) });
        }
        rc
    }

    /// Installs the authorizer (`sqlite3_set_authorizer`). The handler lives
    /// as long as the connection.
    pub fn set_authorizer(&self, func: Box<dyn AuthorizerFunction>) {
        let db = self.raw();
        if db.is_null() {
            return;
        }
        let boxed = Box::new(func);
        let data: *const Box<dyn AuthorizerFunction> = &raw const *boxed;
        // SAFETY: valid handle; `data` points into a Box stored in `self`
        // (heap address stable) that is only dropped after the handle is
        // closed (see `Drop`) or replaced after re-registration below.
        unsafe {
            sys::sqlite3_set_authorizer(db, Some(authorizer_trampoline), data.cast_mut().cast());
        }
        *self.authorizer.borrow_mut() = Some(boxed);
    }

    /// `sqlite3_blob_open()`; on failure returns the result code.
    pub fn blob_open(
        self: &Rc<Self>,
        dbname: &[u8],
        table: &[u8],
        column: &[u8],
        rowid: i64,
        writable: bool,
    ) -> Result<RawBlob, i32> {
        let db = self.raw();
        if db.is_null() {
            return Err(SQLITE_MISUSE);
        }
        let (dbname, table, column) = (c_string(dbname), c_string(table), c_string(column));
        let mut blob: *mut sys::sqlite3_blob = ptr::null_mut();
        // SAFETY: valid handle, C strings and out pointer.
        let rc = unsafe {
            sys::sqlite3_blob_open(
                db,
                dbname.as_ptr(),
                table.as_ptr(),
                column.as_ptr(),
                rowid,
                c_int::from(writable),
                &raw mut blob,
            )
        };
        if rc != SQLITE_OK {
            if !blob.is_null() {
                // SAFETY: release a partially opened handle.
                unsafe { sys::sqlite3_blob_close(blob) };
            }
            return Err(rc);
        }
        Ok(RawBlob {
            blob: Cell::new(blob),
            _conn: Rc::clone(self),
        })
    }

    /// Copies `source_name` of `self` into `dest_name` of `dest` with
    /// `sqlite3_backup_init/step(-1)/finish`, returning the final result
    /// code exactly like `SQLite3::backup()` computes it.
    pub fn backup_to(&self, dest: &Connection, dest_name: &[u8], source_name: &[u8]) -> i32 {
        let (src, dst) = (self.raw(), dest.raw());
        if src.is_null() || dst.is_null() {
            return SQLITE_MISUSE;
        }
        let (dn, sn) = (c_string(dest_name), c_string(source_name));
        // SAFETY: valid handles and names; the backup handle is finished
        // before returning.
        unsafe {
            let b = sys::sqlite3_backup_init(dst, dn.as_ptr(), src, sn.as_ptr());
            if b.is_null() {
                return sys::sqlite3_errcode(src);
            }
            let mut rc;
            loop {
                rc = sys::sqlite3_backup_step(b, -1);
                if rc != SQLITE_OK {
                    break;
                }
            }
            let _ = rc;
            sys::sqlite3_backup_finish(b)
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let db = self.db.get();
        if !db.is_null() {
            // SAFETY: valid handle; all statements and blobs hold an `Rc` to
            // this connection, so none is alive any more.
            unsafe { sqlite3_close_v2(db) };
            self.db.set(ptr::null_mut());
        }
    }
}

/// A prepared statement. A NULL statement (SQL without any statement) is
/// represented too; it behaves like the C API with a NULL pointer.
pub(crate) struct RawStatement {
    stmt: Cell<*mut sys::sqlite3_stmt>,
    conn: Rc<Connection>,
    stepping: Cell<u32>,
    finalize_pending: Cell<bool>,
}

impl std::fmt::Debug for RawStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawStatement")
            .field("null", &self.stmt.get().is_null())
            .finish_non_exhaustive()
    }
}

impl RawStatement {
    fn raw(&self) -> *mut sys::sqlite3_stmt {
        self.stmt.get()
    }

    /// Whether the statement pointer is NULL (never prepared or finalized).
    pub fn is_null(&self) -> bool {
        self.raw().is_null()
    }

    /// `sqlite3_step()`.
    pub fn step(&self) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_MISUSE;
        }
        let _busy = BusyGuard::new(&self.conn.busy);
        self.stepping.set(self.stepping.get() + 1);
        // SAFETY: valid statement; re-entrant finalization is deferred via
        // `stepping` / `finalize_pending`.
        let rc = unsafe { sys::sqlite3_step(s) };
        self.stepping.set(self.stepping.get() - 1);
        if self.stepping.get() == 0 && self.finalize_pending.get() {
            self.finalize();
        }
        rc
    }

    /// `sqlite3_reset()`.
    pub fn reset(&self) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_OK;
        }
        if self.stepping.get() > 0 {
            return SQLITE_MISUSE;
        }
        let _busy = BusyGuard::new(&self.conn.busy);
        // SAFETY: valid statement not currently being stepped.
        unsafe { sys::sqlite3_reset(s) }
    }

    /// `sqlite3_finalize()`; the statement becomes NULL.
    pub fn finalize(&self) {
        let s = self.raw();
        if s.is_null() {
            return;
        }
        if self.stepping.get() > 0 {
            self.finalize_pending.set(true);
            return;
        }
        self.stmt.set(ptr::null_mut());
        self.finalize_pending.set(false);
        // SAFETY: valid statement, finalized exactly once.
        unsafe { sys::sqlite3_finalize(s) };
    }

    /// `sqlite3_clear_bindings()`.
    pub fn clear_bindings(&self) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_clear_bindings(s) }
    }

    /// `sqlite3_bind_null()`.
    pub fn bind_null(&self, idx: i64) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid statement; SQLite range-checks the index.
        unsafe { sys::sqlite3_bind_null(s, c_index(idx)) }
    }

    /// `sqlite3_bind_int64()`.
    pub fn bind_int(&self, idx: i64, v: i64) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_bind_int64(s, c_index(idx), v) }
    }

    /// `sqlite3_bind_double()`.
    pub fn bind_double(&self, idx: i64, v: f64) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_bind_double(s, c_index(idx), v) }
    }

    /// `sqlite3_bind_text(..., SQLITE_TRANSIENT)`.
    pub fn bind_text(&self, idx: i64, v: &[u8]) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid statement and buffer; SQLite copies the data.
        unsafe {
            sys::sqlite3_bind_text(
                s,
                c_index(idx),
                v.as_ptr().cast(),
                c_len(v.len()),
                sys::SQLITE_TRANSIENT(),
            )
        }
    }

    /// `sqlite3_bind_blob(..., SQLITE_TRANSIENT)`.
    pub fn bind_blob(&self, idx: i64, v: &[u8]) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid statement and buffer; SQLite copies the data.
        unsafe {
            sys::sqlite3_bind_blob(
                s,
                c_index(idx),
                v.as_ptr().cast(),
                c_len(v.len()),
                sys::SQLITE_TRANSIENT(),
            )
        }
    }

    /// `sqlite3_bind_parameter_count()`.
    pub fn bind_parameter_count(&self) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return 0;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_bind_parameter_count(s) }
    }

    /// `sqlite3_bind_parameter_index()` (0 when unknown).
    pub fn bind_parameter_index(&self, name: &[u8]) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return 0;
        }
        let name = c_string(name);
        // SAFETY: valid statement and C string.
        unsafe { sys::sqlite3_bind_parameter_index(s, name.as_ptr()) }
    }

    /// `sqlite3_column_count()`.
    pub fn column_count(&self) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return 0;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_column_count(s) }
    }

    /// `sqlite3_data_count()`.
    pub fn data_count(&self) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return 0;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_data_count(s) }
    }

    /// `sqlite3_column_type()` (`SQLITE_NULL` out of range).
    pub fn column_type(&self, col: i32) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_NULL;
        }
        // SAFETY: valid statement; SQLite handles out-of-range columns.
        unsafe { sys::sqlite3_column_type(s, col) }
    }

    /// `sqlite3_column_int64()`.
    pub fn column_int64(&self, col: i32) -> i64 {
        let s = self.raw();
        if s.is_null() {
            return 0;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_column_int64(s, col) }
    }

    /// `sqlite3_column_double()`.
    pub fn column_double(&self, col: i32) -> f64 {
        let s = self.raw();
        if s.is_null() {
            return 0.0;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_column_double(s, col) }
    }

    /// `sqlite3_column_text()` + `sqlite3_column_bytes()` (`None` for a NULL
    /// pointer).
    pub fn column_text(&self, col: i32) -> Option<Vec<u8>> {
        let s = self.raw();
        if s.is_null() {
            return None;
        }
        // SAFETY: valid statement; text pointer is read before any other
        // call and copied (bytes() after text() as documented).
        unsafe {
            let p = sys::sqlite3_column_text(s, col);
            if p.is_null() {
                return None;
            }
            let n = sys::sqlite3_column_bytes(s, col);
            Some(bytes_at(p.cast(), n))
        }
    }

    /// `sqlite3_column_blob()` + `sqlite3_column_bytes()` (`None` for a NULL
    /// pointer, which includes zero-length BLOBs).
    pub fn column_blob(&self, col: i32) -> Option<Vec<u8>> {
        let s = self.raw();
        if s.is_null() {
            return None;
        }
        // SAFETY: as for column_text.
        unsafe {
            let p = sys::sqlite3_column_blob(s, col);
            if p.is_null() {
                return None;
            }
            let n = sys::sqlite3_column_bytes(s, col);
            Some(bytes_at(p, n))
        }
    }

    /// `sqlite3_column_name()`.
    pub fn column_name(&self, col: i32) -> Option<Vec<u8>> {
        let s = self.raw();
        if s.is_null() {
            return None;
        }
        // SAFETY: valid statement; NULL for out-of-range columns.
        opt_bytes(unsafe { sys::sqlite3_column_name(s, col) })
    }

    /// `sqlite3_column_decltype()`.
    pub fn column_decltype(&self, col: i32) -> Option<Vec<u8>> {
        let s = self.raw();
        if s.is_null() {
            return None;
        }
        // SAFETY: valid statement.
        opt_bytes(unsafe { sys::sqlite3_column_decltype(s, col) })
    }

    /// `sqlite3_column_table_name()`.
    pub fn column_table_name(&self, col: i32) -> Option<Vec<u8>> {
        let s = self.raw();
        if s.is_null() {
            return None;
        }
        // SAFETY: valid statement (the system library is built with
        // SQLITE_ENABLE_COLUMN_METADATA).
        opt_bytes(unsafe { sys::sqlite3_column_table_name(s, col) })
    }

    /// `sqlite3_sql()`.
    pub fn sql(&self) -> Vec<u8> {
        let s = self.raw();
        if s.is_null() {
            return Vec::new();
        }
        // SAFETY: valid statement.
        opt_bytes(unsafe { sys::sqlite3_sql(s) }).unwrap_or_default()
    }

    /// `sqlite3_expanded_sql()`.
    pub fn expanded_sql(&self) -> Vec<u8> {
        let s = self.raw();
        if s.is_null() {
            return Vec::new();
        }
        // SAFETY: valid statement; the result is copied and freed.
        unsafe {
            let p = sys::sqlite3_expanded_sql(s);
            let out = opt_bytes(p).unwrap_or_default();
            if !p.is_null() {
                sys::sqlite3_free(p.cast());
            }
            out
        }
    }

    /// `sqlite3_stmt_readonly()`.
    pub fn readonly(&self) -> bool {
        let s = self.raw();
        // SAFETY: valid statement or NULL (accepted, returns true).
        s.is_null() || unsafe { sys::sqlite3_stmt_readonly(s) != 0 }
    }

    /// `sqlite3_stmt_busy()`.
    pub fn busy(&self) -> bool {
        let s = self.raw();
        // SAFETY: valid statement (NULL returns 0).
        !s.is_null() && unsafe { sys::sqlite3_stmt_busy(s) != 0 }
    }

    /// `sqlite3_stmt_isexplain()`.
    pub fn isexplain(&self) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return 0;
        }
        // SAFETY: valid statement.
        unsafe { sys::sqlite3_stmt_isexplain(s) }
    }

    /// `sqlite3_stmt_explain()`.
    pub fn explain(&self, mode: i32) -> i32 {
        let s = self.raw();
        if s.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid statement; available since SQLite 3.43 (system
        // library is 3.45).
        unsafe { sqlite3_stmt_explain(s, mode) }
    }

    /// `sqlite3_errcode(sqlite3_db_handle(stmt))`.
    pub fn db_errcode(&self) -> i32 {
        if self.is_null() {
            return errcode_null();
        }
        self.conn.errcode()
    }

    /// `sqlite3_errmsg(sqlite3_db_handle(stmt))`.
    pub fn db_errmsg(&self) -> Vec<u8> {
        if self.is_null() {
            return errmsg_null();
        }
        self.conn.errmsg()
    }
}

impl Drop for RawStatement {
    fn drop(&mut self) {
        let s = self.stmt.get();
        if !s.is_null() {
            // SAFETY: valid statement finalized exactly once.
            unsafe { sys::sqlite3_finalize(s) };
            self.stmt.set(ptr::null_mut());
        }
    }
}

/// `int` conversion of a parameter number as C does it (implicit
/// truncation of a `zend_long`).
#[allow(clippy::cast_possible_truncation)]
fn c_index(idx: i64) -> c_int {
    idx as c_int
}

/// An open BLOB handle.
pub(crate) struct RawBlob {
    blob: Cell<*mut sys::sqlite3_blob>,
    _conn: Rc<Connection>,
}

impl std::fmt::Debug for RawBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawBlob")
            .field("open", &!self.blob.get().is_null())
            .finish_non_exhaustive()
    }
}

impl RawBlob {
    /// `sqlite3_blob_bytes()`.
    pub fn bytes(&self) -> usize {
        let b = self.blob.get();
        if b.is_null() {
            return 0;
        }
        // SAFETY: valid blob handle.
        usize::try_from(unsafe { sys::sqlite3_blob_bytes(b) }).unwrap_or(0)
    }

    /// `sqlite3_blob_read(blob, buf, len, offset)`.
    pub fn read(&self, buf: &mut [u8], offset: usize) -> i32 {
        let b = self.blob.get();
        if b.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid handle; `buf` is writable for its length.
        unsafe {
            sys::sqlite3_blob_read(b, buf.as_mut_ptr().cast(), c_len(buf.len()), c_len(offset))
        }
    }

    /// `sqlite3_blob_write(blob, buf, len, offset)`.
    pub fn write(&self, buf: &[u8], offset: usize) -> i32 {
        let b = self.blob.get();
        if b.is_null() {
            return SQLITE_MISUSE;
        }
        // SAFETY: valid handle; `buf` is readable for its length.
        unsafe { sys::sqlite3_blob_write(b, buf.as_ptr().cast(), c_len(buf.len()), c_len(offset)) }
    }

    /// `sqlite3_blob_close()`.
    pub fn close(&self) {
        let b = self.blob.get();
        if !b.is_null() {
            self.blob.set(ptr::null_mut());
            // SAFETY: valid handle closed exactly once.
            unsafe { sys::sqlite3_blob_close(b) };
        }
    }
}

impl Drop for RawBlob {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------
// Callback trampolines
// ---------------------------------------------------------------------------

/// Reads the arguments of a user-defined function call.
///
/// # Safety
/// `argv` must point to `argc` valid `sqlite3_value*` (as passed by `SQLite`).
unsafe fn read_args(argc: c_int, argv: *mut *mut sys::sqlite3_value) -> Vec<ArgValue> {
    let n = usize::try_from(argc).unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    if argv.is_null() {
        return out;
    }
    for i in 0..n {
        // SAFETY: guaranteed by the caller.
        let v = unsafe { *argv.add(i) };
        // SAFETY: `v` is a valid protected value for the call's duration.
        let arg = unsafe {
            match sys::sqlite3_value_type(v) {
                SQLITE_INTEGER => ArgValue::Int(sys::sqlite3_value_int64(v)),
                SQLITE_FLOAT => ArgValue::Float(sys::sqlite3_value_double(v)),
                SQLITE_NULL => ArgValue::Null,
                _ => {
                    let p = sys::sqlite3_value_text(v);
                    let len = sys::sqlite3_value_bytes(v);
                    ArgValue::Text(bytes_at(p.cast(), len))
                }
            }
        };
        out.push(arg);
    }
    out
}

/// Applies a function result.
///
/// # Safety
/// `ctx` must be the context passed by `SQLite` to the running callback.
unsafe fn set_result(ctx: *mut sys::sqlite3_context, r: FnResult) {
    // SAFETY: guaranteed by the caller; buffers are copied (TRANSIENT).
    unsafe {
        match r {
            FnResult::Int(i) => sys::sqlite3_result_int64(ctx, i),
            FnResult::Float(f) => sys::sqlite3_result_double(ctx, f),
            FnResult::Null => sys::sqlite3_result_null(ctx),
            FnResult::Text(t) => {
                sys::sqlite3_result_text(
                    ctx,
                    t.as_ptr().cast(),
                    c_len(t.len()),
                    sys::SQLITE_TRANSIENT(),
                );
            }
            FnResult::Error(msg, len) => {
                let len = len.min(c_len(msg.len()));
                sys::sqlite3_result_error(ctx, msg.as_ptr().cast(), len);
            }
            FnResult::Unset => {}
        }
    }
}

unsafe extern "C" fn scalar_trampoline(
    ctx: *mut sys::sqlite3_context,
    argc: c_int,
    argv: *mut *mut sys::sqlite3_value,
) {
    // SAFETY: user data is the `UserFunction` registered in
    // `create_function`, alive until `destroy_function`.
    let func = unsafe { &*sys::sqlite3_user_data(ctx).cast::<UserFunction>() };
    // SAFETY: argv/argc come from SQLite.
    let args = unsafe { read_args(argc, argv) };
    let r = catch_unwind(AssertUnwindSafe(|| match func {
        UserFunction::Scalar(f) => f.call(args),
        UserFunction::Aggregate(_) => FnResult::Unset,
    }))
    .unwrap_or_else(|_| FnResult::Error(b"failed to invoke callback".to_vec(), 0));
    // SAFETY: ctx from SQLite.
    unsafe { set_result(ctx, r) };
}

/// Fetches (allocating on first use) the aggregate state of the group.
///
/// # Safety
/// `ctx` must be the context of a running aggregate callback.
unsafe fn agg_state<'a>(ctx: *mut sys::sqlite3_context) -> Option<&'a mut AggregateState> {
    // SAFETY: SQLite returns zeroed per-group memory of the requested size
    // (or NULL on OOM) that lives until xFinal returns; we store one
    // pointer to a leaked Box in it.
    unsafe {
        let slot =
            sys::sqlite3_aggregate_context(ctx, c_len(std::mem::size_of::<*mut AggregateState>()))
                .cast::<*mut AggregateState>();
        if slot.is_null() {
            return None;
        }
        if (*slot).is_null() {
            *slot = Box::into_raw(Box::default());
        }
        Some(&mut **slot)
    }
}

unsafe extern "C" fn step_trampoline(
    ctx: *mut sys::sqlite3_context,
    argc: c_int,
    argv: *mut *mut sys::sqlite3_value,
) {
    // SAFETY: see scalar_trampoline.
    let func = unsafe { &*sys::sqlite3_user_data(ctx).cast::<UserFunction>() };
    // SAFETY: argv/argc come from SQLite.
    let args = unsafe { read_args(argc, argv) };
    // SAFETY: ctx is an aggregate context.
    let Some(state) = (unsafe { agg_state(ctx) }) else {
        return;
    };
    let r = catch_unwind(AssertUnwindSafe(|| match func {
        UserFunction::Aggregate(f) => f.step(state, args),
        UserFunction::Scalar(_) => FnResult::Unset,
    }))
    .unwrap_or(FnResult::Unset);
    // SAFETY: ctx from SQLite.
    unsafe { set_result(ctx, r) };
}

unsafe extern "C" fn final_trampoline(ctx: *mut sys::sqlite3_context) {
    // SAFETY: see scalar_trampoline.
    let func = unsafe { &*sys::sqlite3_user_data(ctx).cast::<UserFunction>() };
    // SAFETY: ctx is an aggregate context; the slot is read and cleared so
    // the Box is reclaimed exactly once.
    let boxed = unsafe {
        let slot =
            sys::sqlite3_aggregate_context(ctx, c_len(std::mem::size_of::<*mut AggregateState>()))
                .cast::<*mut AggregateState>();
        if slot.is_null() {
            None
        } else {
            let p = *slot;
            *slot = ptr::null_mut();
            if p.is_null() {
                Some(Box::<AggregateState>::default())
            } else {
                Some(Box::from_raw(p))
            }
        }
    };
    let Some(mut state) = boxed else {
        return;
    };
    let r = catch_unwind(AssertUnwindSafe(|| match func {
        UserFunction::Aggregate(f) => f.finalize(&mut state),
        UserFunction::Scalar(_) => FnResult::Unset,
    }))
    .unwrap_or(FnResult::Unset);
    drop(state);
    // SAFETY: ctx from SQLite.
    unsafe { set_result(ctx, r) };
}

unsafe extern "C" fn destroy_function(data: *mut c_void) {
    if !data.is_null() {
        // SAFETY: `data` is the Box leaked in `create_function`; SQLite
        // calls this exactly once.
        drop(unsafe { Box::from_raw(data.cast::<UserFunction>()) });
    }
}

unsafe extern "C" fn collation_trampoline(
    data: *mut c_void,
    a_len: c_int,
    a: *const c_void,
    b_len: c_int,
    b: *const c_void,
) -> c_int {
    // SAFETY: `data` is the Box registered in `create_collation`.
    let func = unsafe { &*data.cast::<Box<dyn CollationFunction>>() };
    let (a, b) = (bytes_at(a, a_len), bytes_at(b, b_len));
    catch_unwind(AssertUnwindSafe(|| func.compare(&a, &b))).unwrap_or(0)
}

unsafe extern "C" fn destroy_collation(data: *mut c_void) {
    if !data.is_null() {
        // SAFETY: the Box leaked in `create_collation`, released once.
        drop(unsafe { Box::from_raw(data.cast::<Box<dyn CollationFunction>>()) });
    }
}

unsafe extern "C" fn authorizer_trampoline(
    data: *mut c_void,
    action: c_int,
    a1: *const c_char,
    a2: *const c_char,
    a3: *const c_char,
    a4: *const c_char,
) -> c_int {
    // SAFETY: `data` points to the Box owned by the Connection.
    let func = unsafe { &*data.cast::<Box<dyn AuthorizerFunction>>() };
    let (a1, a2, a3, a4) = (opt_bytes(a1), opt_bytes(a2), opt_bytes(a3), opt_bytes(a4));
    catch_unwind(AssertUnwindSafe(|| {
        func.authorize(
            action,
            [a1.as_deref(), a2.as_deref(), a3.as_deref(), a4.as_deref()],
        )
    }))
    .unwrap_or(SQLITE_DENY)
}
