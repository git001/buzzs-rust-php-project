// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/sqlite3`: the `SQLite3`, `SQLite3Stmt` and `SQLite3Result` classes.
//!
//! Each PHP object maps to a Rust handle ([`Sqlite3`], [`Sqlite3Stmt`],
//! [`Sqlite3Result`]) that the VM stores inside its object. Methods take the
//! [`Host`] (for user callbacks, diagnostics and conversions) and return
//! `Result<_, PhpError>`; non-fatal problems are reported as warnings through
//! [`Host::docref`] (or thrown as `SQLite3Exception` once
//! `enableExceptions(true)` was called), exactly like `php_sqlite3_error()`.
//!
//! Argument parsing (ZPP) and callable validation stay in the VM; the
//! methods here start after `zend_parse_parameters()` succeeded.

use crate::blob::{BlobFlavor, BlobStream};
use crate::constants::{SQLITE3_ASSOC, SQLITE3_NUM, SQLITE3_OPEN_READWRITE};
use crate::conv::{
    arg_to_value, failed_to_invoke, nullable_str, set_slot, sqlite3_column_value, value_to_result,
    ColumnValue,
};
use crate::error::{ExceptionClass, PhpError};
use crate::ffi::{
    self, AggregateFunction, AggregateState, ArgValue, AuthorizerFunction, CollationFunction,
    Connection, FnResult, RawStatement, ScalarFunction, UserFunction, SQLITE_ATTACH, SQLITE_BLOB,
    SQLITE_BUSY, SQLITE_DENY, SQLITE_DONE, SQLITE_ERROR, SQLITE_FLOAT, SQLITE_IGNORE,
    SQLITE_INTEGER, SQLITE_LOCKED, SQLITE_NOMEM, SQLITE_NULL, SQLITE_OK, SQLITE_ROW, SQLITE_TEXT,
    SQLITE_UTF8,
};
use crate::host::{Cx, Host};
use php_runtime::{ArrayKey, ErrorLevel, Value, ZArray, ZStr};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

const NOT_INIT_DB: &str =
    "The SQLite3 object has not been correctly initialised or is already closed";
const NOT_INIT_STMT: &str =
    "The SQLite3Stmt object has not been correctly initialised or is already closed";
const NOT_INIT_RESULT: &str =
    "The SQLite3Result object has not been correctly initialised or is already closed";

/// State shared between a database object and its registered callbacks.
#[derive(Debug, Default)]
struct DbShared {
    /// `enableExceptions()` state.
    exception: Cell<bool>,
    /// Number of user callbacks currently running.
    in_callback: Cell<u32>,
    /// The `setAuthorizer()` callable.
    authorizer: RefCell<Option<Value>>,
}

/// `php_sqlite3_error()`: throws `SQLite3Exception` (code `errcode`) when
/// exceptions are enabled on `db`, otherwise emits a warning.
fn sqlite_error(cx: &mut Cx<'_>, db: Option<&DbShared>, errcode: i32, msg: &str) {
    if db.is_some_and(|d| d.exception.get()) {
        cx.throw_new(ExceptionClass::Sqlite3Exception, msg, i64::from(errcode));
    } else {
        cx.warning(msg);
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

struct InCallback<'a>(&'a Cell<u32>);

impl<'a> InCallback<'a> {
    fn new(c: &'a Cell<u32>) -> Self {
        c.set(c.get() + 1);
        InCallback(c)
    }
}

impl Drop for InCallback<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

struct Scalar {
    callable: Value,
    shared: Rc<DbShared>,
}

impl ScalarFunction for Scalar {
    fn call(&self, args: Vec<ArgValue>) -> FnResult {
        ffi::with_scope(|cx| {
            let Some(cx) = cx else {
                return failed_to_invoke();
            };
            let ret = {
                let _g = InCallback::new(&self.shared.in_callback);
                let zargs: Vec<Value> = args.into_iter().map(arg_to_value).collect();
                cx.call(&self.callable, &zargs)
            };
            match ret {
                Some(v) => value_to_result(cx, &v),
                None => failed_to_invoke(),
            }
        })
    }
}

struct Aggregate {
    step: Value,
    fini: Value,
    shared: Rc<DbShared>,
}

/// Outcome of invoking an aggregate callback.
enum Invoked {
    /// No call context (callback outside any API call).
    NoScope,
    /// The return value (`None` is `UNDEF`).
    Returned(Option<Value>),
}

impl Aggregate {
    /// `sqlite3_do_callback(..., is_agg)`.
    fn invoke(&self, callable: &Value, state: &mut AggregateState, args: Vec<ArgValue>) -> Invoked {
        ffi::with_scope(|cx| {
            let Some(cx) = cx else {
                return Invoked::NoScope;
            };
            let ret = {
                let _g = InCallback::new(&self.shared.in_callback);
                let mut zargs = Vec::with_capacity(args.len() + 2);
                zargs.push(state.context.clone().unwrap_or(Value::Null));
                zargs.push(Value::Int(state.rows));
                zargs.extend(args.into_iter().map(arg_to_value));
                cx.call(callable, &zargs)
            };
            Invoked::Returned(ret)
        })
    }
}

impl AggregateFunction for Aggregate {
    fn step(&self, state: &mut AggregateState, args: Vec<ArgValue>) -> FnResult {
        state.rows += 1;
        if let Invoked::Returned(ret) = self.invoke(&self.step, state, args) {
            // The return value becomes the new context (UNDEF on failure).
            state.context = ret;
        }
        FnResult::Unset
    }

    fn finalize(&self, state: &mut AggregateState) -> FnResult {
        state.rows = 0;
        let fini = self.fini.clone();
        let Invoked::Returned(ret) = self.invoke(&fini, state, Vec::new()) else {
            return failed_to_invoke();
        };
        let r = match ret {
            Some(v) => ffi::with_scope(|cx| match cx {
                Some(cx) => value_to_result(cx, &v),
                None => FnResult::Unset,
            }),
            None => failed_to_invoke(),
        };
        state.context = None;
        r
    }
}

struct Collation {
    callable: Value,
    shared: Rc<DbShared>,
}

impl CollationFunction for Collation {
    fn compare(&self, a: &[u8], b: &[u8]) -> i32 {
        ffi::with_scope(|cx| {
            let Some(cx) = cx else {
                return 0;
            };
            // Exception occurred on a previous callback: don't call again.
            if cx.has_exception() {
                return 0;
            }
            let ret = {
                let _g = InCallback::new(&self.shared.in_callback);
                cx.call(
                    &self.callable,
                    &[Value::Str(ZStr::from(a)), Value::Str(ZStr::from(b))],
                )
            };
            if cx.has_exception() {
                return 0;
            }
            if let Some(Value::Int(i)) = ret {
                #[allow(clippy::cast_possible_truncation)]
                return i as i32;
            }
            sqlite_error(
                cx,
                None,
                0,
                "An error occurred while invoking the compare callback (invalid return type).  Collation behaviour is undefined.",
            );
            0
        })
    }
}

struct Authorizer {
    shared: Rc<DbShared>,
}

impl AuthorizerFunction for Authorizer {
    fn authorize(&self, action: i32, args: [Option<&[u8]>; 4]) -> i32 {
        ffi::with_scope(|cx| {
            let callable = self.shared.authorizer.borrow().clone();
            let Some(cx) = cx else {
                return if callable.is_some() {
                    SQLITE_DENY
                } else {
                    SQLITE_OK
                };
            };
            // Check open_basedir restrictions first.
            if cx.host().open_basedir_active() && action == SQLITE_ATTACH {
                let Some(arg1) = args[0] else {
                    return SQLITE_DENY;
                };
                if arg1 != b":memory:"
                    && !arg1.is_empty()
                    && (arg1.starts_with(b"file:") || cx.host().check_open_basedir(arg1))
                {
                    return SQLITE_DENY;
                }
            }
            let Some(callable) = callable else {
                return SQLITE_OK;
            };
            let argv = [
                Value::Int(i64::from(action)),
                nullable_str(args[0]),
                nullable_str(args[1]),
                nullable_str(args[2]),
                nullable_str(args[3]),
            ];
            let ret = {
                let _g = InCallback::new(&self.shared.in_callback);
                cx.call(&callable, &argv)
            };
            let shared = Some(&*self.shared);
            match ret {
                None => {
                    sqlite_error(
                        cx,
                        shared,
                        0,
                        "An error occurred while invoking the authorizer callback",
                    );
                    SQLITE_DENY
                }
                Some(Value::Int(v)) => {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = v as i32;
                    if v != SQLITE_OK && v != SQLITE_IGNORE && v != SQLITE_DENY {
                        sqlite_error(
                            cx,
                            shared,
                            0,
                            &format!("The authorizer callback returned an invalid value: {v}"),
                        );
                        SQLITE_DENY
                    } else {
                        v
                    }
                }
                Some(_) => {
                    sqlite_error(
                        cx,
                        shared,
                        0,
                        "The authorizer callback returned an invalid type: expected int",
                    );
                    SQLITE_DENY
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// SQLite3
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct DbObject {
    conn: RefCell<Option<Rc<Connection>>>,
    initialised: Cell<bool>,
    shared: Rc<DbShared>,
    free_list: RefCell<Vec<Weak<StmtObject>>>,
}

impl DbObject {
    fn conn(&self) -> Option<Rc<Connection>> {
        self.conn.borrow().clone()
    }

    /// `SQLITE3_CHECK_INITIALIZED(db_obj, db_obj->initialised, SQLite3)`.
    fn check_init(&self) -> Result<Rc<Connection>, PhpError> {
        match self.conn() {
            Some(c) if self.initialised.get() => Ok(c),
            _ => Err(PhpError::error(NOT_INIT_DB)),
        }
    }

    /// `SQLITE3_CHECK_INITIALIZED(db_obj, db_obj->db, SQLite3)`.
    fn check_db(&self) -> Result<Rc<Connection>, PhpError> {
        self.conn().ok_or_else(|| PhpError::error(NOT_INIT_DB))
    }

    /// `zend_llist_del_element(&free_list, stmt, php_sqlite3_compare_stmt_free)`.
    fn free_list_remove(&self, stmt: &StmtObject) {
        if !stmt.initialised.get() {
            return;
        }
        let mut list = self.free_list.borrow_mut();
        if let Some(pos) = list.iter().position(|w| std::ptr::eq(w.as_ptr(), stmt)) {
            list.remove(pos);
            drop(list);
            stmt.stmt.finalize();
            stmt.initialised.set(false);
        }
    }

    /// `zend_llist_clean(&free_list)`.
    fn free_list_clean(&self) {
        let list = std::mem::take(&mut *self.free_list.borrow_mut());
        for w in list {
            if let Some(s) = w.upgrade() {
                if s.initialised.get() {
                    s.stmt.finalize();
                    s.initialised.set(false);
                }
            }
        }
    }
}

impl Drop for DbObject {
    fn drop(&mut self) {
        // php_sqlite3_object_free_storage: the connection is closed with
        // sqlite3_close_v2() once the last statement/blob releases it.
        self.shared.authorizer.borrow_mut().take();
        self.initialised.set(false);
    }
}

/// A `SQLite3` object.
#[derive(Debug, Clone)]
pub struct Sqlite3 {
    db: Rc<DbObject>,
}

impl Default for Sqlite3 {
    fn default() -> Self {
        Self::new()
    }
}

/// The `$param` of `bindParam()` / `bindValue()` (`string|int`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKey<'a> {
    /// A parameter name (`:name`, `@name` or `name`).
    Name(&'a [u8]),
    /// A 1-based parameter number.
    Number(i64),
}

impl Sqlite3 {
    /// A new, not yet opened object (`create_object`; what
    /// `newInstanceWithoutConstructor()` yields).
    pub fn new() -> Self {
        Sqlite3 {
            db: Rc::new(DbObject {
                conn: RefCell::new(None),
                initialised: Cell::new(false),
                shared: Rc::new(DbShared::default()),
                free_list: RefCell::new(Vec::new()),
            }),
        }
    }

    /// `new SQLite3($filename, $flags, $encryptionKey)`.
    pub fn construct(
        host: &mut dyn Host,
        filename: &[u8],
        flags: i64,
        encryption_key: &[u8],
    ) -> Result<Self, PhpError> {
        let db = Sqlite3::new();
        db.open(host, filename, flags, encryption_key)?;
        Ok(db)
    }

    /// Whether two handles refer to the same PHP object.
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        Rc::ptr_eq(&a.db, &b.db)
    }

    /// Whether the database is open (`initialised`).
    pub fn is_open(&self) -> bool {
        self.db.initialised.get()
    }

    /// `SQLite3::open()`. The encryption key is ignored (this build has no
    /// `SQLITE_HAS_CODEC`, like stock PHP builds).
    pub fn open(
        &self,
        host: &mut dyn Host,
        filename: &[u8],
        flags: i64,
        encryption_key: &[u8],
    ) -> Result<(), PhpError> {
        let _ = encryption_key;
        let db = &self.db;
        if db.initialised.get() {
            return Err(PhpError::new(
                ExceptionClass::Exception,
                "Already initialised DB Object",
                0,
            ));
        }
        let fullpath = if !filename.is_empty() && filename != b":memory:" {
            let Some(full) = host.expand_filepath(filename) else {
                return Err(PhpError::new(
                    ExceptionClass::Exception,
                    "Unable to expand filepath",
                    0,
                ));
            };
            if host.check_open_basedir(&full) {
                return Err(PhpError::new(
                    ExceptionClass::Exception,
                    format!("open_basedir prohibits opening {}", lossy(&full)),
                    0,
                ));
            }
            full
        } else {
            filename.to_vec()
        };
        #[allow(clippy::cast_possible_truncation)]
        let conn = match Connection::open(&fullpath, flags as i32) {
            Ok(c) => c,
            Err(e) => {
                return Err(PhpError::new(
                    ExceptionClass::Exception,
                    format!("Unable to open database: {}", lossy(&e.message)),
                    0,
                ))
            }
        };
        db.initialised.set(true);
        *db.shared.authorizer.borrow_mut() = None;
        conn.set_authorizer(Box::new(Authorizer {
            shared: Rc::clone(&db.shared),
        }));
        if host.sqlite3_defensive() {
            conn.set_defensive(true);
        }
        *db.conn.borrow_mut() = Some(conn);
        Ok(())
    }

    /// `SQLite3::close()`.
    pub fn close(&self, host: &mut dyn Host) -> Result<bool, PhpError> {
        let db = &self.db;
        if !db.initialised.get() {
            return Ok(true);
        }
        if db.shared.in_callback.get() > 0 {
            return Err(PhpError::error(
                "Cannot close SQLite3 database while inside a callback",
            ));
        }
        db.free_list_clean();
        if let Some(conn) = db.conn() {
            let rc = conn.close();
            if rc != SQLITE_OK {
                let mut cx = Cx::new(host);
                let msg = format!("Unable to close database: {}", lossy(&conn.errmsg()));
                sqlite_error(&mut cx, Some(&db.shared), rc, &msg);
                return cx.finish(false);
            }
        }
        db.initialised.set(false);
        Ok(true)
    }

    /// `SQLite3::exec()`.
    pub fn exec(&self, host: &mut dyn Host, sql: &[u8]) -> Result<bool, PhpError> {
        let conn = self.db.check_init()?;
        let mut cx = Cx::new(host);
        let r = ffi::enter(&mut cx, || conn.exec(sql));
        if let Err((code, msg)) = r {
            let msg = msg.map_or_else(|| "(null)".to_string(), |m| lossy(&m));
            sqlite_error(&mut cx, Some(&self.db.shared), code, &msg);
            return cx.finish(false);
        }
        cx.finish(true)
    }

    /// `SQLite3::version()`: `["versionString" => ..., "versionNumber" => ...]`.
    pub fn version() -> ZArray {
        let mut arr = ZArray::new();
        arr.make_mut().insert(
            ArrayKey::from_bytes(b"versionString"),
            Value::from(ffi::libversion()),
        );
        arr.make_mut().insert(
            ArrayKey::from_bytes(b"versionNumber"),
            Value::Int(i64::from(ffi::libversion_number())),
        );
        arr
    }

    /// `SQLite3::lastInsertRowID()`.
    pub fn last_insert_row_id(&self) -> Result<i64, PhpError> {
        Ok(self.db.check_init()?.last_insert_rowid())
    }

    /// `SQLite3::lastErrorCode()`.
    pub fn last_error_code(&self) -> Result<i64, PhpError> {
        let conn = self.db.check_db()?;
        Ok(if self.db.initialised.get() {
            i64::from(conn.errcode())
        } else {
            0
        })
    }

    /// `SQLite3::lastExtendedErrorCode()`.
    pub fn last_extended_error_code(&self) -> Result<i64, PhpError> {
        let conn = self.db.check_db()?;
        Ok(if self.db.initialised.get() {
            i64::from(conn.extended_errcode())
        } else {
            0
        })
    }

    /// `SQLite3::enableExtendedResultCodes()`.
    pub fn enable_extended_result_codes(&self, enable: bool) -> Result<bool, PhpError> {
        let conn = self.db.check_db()?;
        Ok(self.db.initialised.get() && conn.extended_result_codes(i64::from(enable)) == SQLITE_OK)
    }

    /// `SQLite3::lastErrorMsg()`.
    pub fn last_error_msg(&self) -> Result<ZStr, PhpError> {
        let conn = self.db.check_db()?;
        Ok(if self.db.initialised.get() {
            ZStr::from(conn.errmsg())
        } else {
            ZStr::empty()
        })
    }

    /// `SQLite3::busyTimeout()`.
    pub fn busy_timeout(&self, ms: i64) -> Result<bool, PhpError> {
        let conn = self.db.check_init()?;
        // Without SQLITE_ENABLE_API_ARMOR the result is ignored.
        let _ = conn.busy_timeout(ms);
        Ok(true)
    }

    /// `SQLite3::loadExtension()` (honours `sqlite3.extension_dir`).
    pub fn load_extension(&self, host: &mut dyn Host, name: &[u8]) -> Result<bool, PhpError> {
        let conn = self.db.check_init()?;
        let shared = Some(&*self.db.shared);
        let mut cx = Cx::new(host);
        let Some(dir) = cx.host().sqlite3_extension_dir() else {
            sqlite_error(&mut cx, shared, 0, "SQLite Extensions are disabled");
            return cx.finish(false);
        };
        if name.is_empty() {
            return Err(PhpError::new(
                ExceptionClass::ValueError,
                "SQLite3::loadExtension(): Argument #1 ($name) must not be empty",
                0,
            ));
        }
        let mut lib_path = dir.clone();
        if !matches!(dir.last(), Some(b'/')) {
            lib_path.push(b'/');
        }
        lib_path.extend_from_slice(name);
        let Some(fullpath) = cx.host().realpath(&lib_path) else {
            let msg = format!("Unable to load extension at '{}'", lossy(&lib_path));
            sqlite_error(&mut cx, shared, 0, &msg);
            return cx.finish(false);
        };
        if !fullpath.starts_with(&dir) {
            sqlite_error(
                &mut cx,
                shared,
                0,
                "Unable to open extensions outside the defined directory",
            );
            return cx.finish(false);
        }
        if let Err(msg) = conn.load_extension(&fullpath, false) {
            let msg = msg.map_or_else(|| "(null)".to_string(), |m| lossy(&m));
            sqlite_error(&mut cx, shared, 0, &msg);
            return cx.finish(false);
        }
        cx.finish(true)
    }

    /// `SQLite3::changes()`.
    pub fn changes(&self) -> Result<i64, PhpError> {
        Ok(self.db.check_init()?.changes())
    }

    /// `SQLite3::escapeString()` (single quotes doubled).
    pub fn escape_string(s: &[u8]) -> Result<ZStr, PhpError> {
        if s.contains(&0) {
            return Err(PhpError::new(
                ExceptionClass::ValueError,
                "SQLite3::escapeString(): Argument #1 ($string) must not contain any null bytes",
                0,
            ));
        }
        if s.is_empty() {
            return Ok(ZStr::empty());
        }
        match ffi::escape_q(s) {
            Some(out) => Ok(ZStr::from(out)),
            None => Err(PhpError::new(
                ExceptionClass::Sqlite3Exception,
                "Unable to escape string",
                0,
            )),
        }
    }

    fn new_stmt(&self, raw: RawStatement) -> Rc<StmtObject> {
        Rc::new(StmtObject {
            db: Rc::clone(&self.db),
            stmt: Rc::new(raw),
            initialised: Cell::new(true),
            bound: RefCell::new(Vec::new()),
        })
    }

    fn register(&self, stmt: &Rc<StmtObject>) {
        self.db.free_list.borrow_mut().push(Rc::downgrade(stmt));
    }

    /// `SQLite3::prepare()` (and `SQLite3Stmt::__construct`): `None` is
    /// `false`.
    pub fn prepare(
        &self,
        host: &mut dyn Host,
        sql: &[u8],
    ) -> Result<Option<Sqlite3Stmt>, PhpError> {
        let conn = self.db.check_init()?;
        if sql.is_empty() {
            return Ok(None);
        }
        let mut cx = Cx::new(host);
        let r = ffi::enter(&mut cx, || conn.prepare(sql));
        match r {
            Ok(raw) => {
                let obj = self.new_stmt(raw);
                self.register(&obj);
                cx.finish(Some(Sqlite3Stmt { obj }))
            }
            Err(code) => {
                let msg = format!("Unable to prepare statement: {}", lossy(&conn.errmsg()));
                sqlite_error(&mut cx, Some(&self.db.shared), code, &msg);
                cx.finish(None)
            }
        }
    }

    /// The `sqlite3_exec()` path taken by `query()`/`querySingle()` when
    /// the return value is unused.
    fn exec_unused(&self, cx: &mut Cx<'_>, conn: &Connection, sql: &[u8]) {
        let r = ffi::enter(cx, || conn.exec(sql));
        if let Err((code, msg)) = r {
            let msg = msg.map_or_else(|| "(null)".to_string(), |m| lossy(&m));
            sqlite_error(cx, Some(&self.db.shared), code, &msg);
        }
    }

    /// `SQLite3::query()`. `used_ret` mirrors `USED_RET()`: when the result
    /// is discarded the SQL is run with `sqlite3_exec()` and `false` is
    /// returned. `None` is `false`.
    pub fn query(
        &self,
        host: &mut dyn Host,
        sql: &[u8],
        used_ret: bool,
    ) -> Result<Option<Sqlite3Result>, PhpError> {
        let conn = self.db.check_init()?;
        if sql.is_empty() {
            return Ok(None);
        }
        let mut cx = Cx::new(host);
        if !used_ret {
            self.exec_unused(&mut cx, &conn, sql);
            return cx.finish(None);
        }
        let raw = match ffi::enter(&mut cx, || conn.prepare(sql)) {
            Ok(raw) => raw,
            Err(code) => {
                let msg = format!("Unable to prepare statement: {}", lossy(&conn.errmsg()));
                sqlite_error(&mut cx, Some(&self.db.shared), code, &msg);
                return cx.finish(None);
            }
        };
        let obj = self.new_stmt(raw);
        let stmt = Rc::clone(&obj.stmt);
        let rc = ffi::enter(&mut cx, || stmt.step());
        if rc == SQLITE_ROW || rc == SQLITE_DONE {
            self.register(&obj);
            stmt.reset();
            cx.finish(Some(Sqlite3Result::new(obj, false)))
        } else {
            if !cx.has_exception() {
                let msg = format!("Unable to execute statement: {}", lossy(&conn.errmsg()));
                sqlite_error(&mut cx, Some(&self.db.shared), conn.errcode(), &msg);
            }
            stmt.finalize();
            obj.initialised.set(false);
            cx.finish(None)
        }
    }

    /// `SQLite3::querySingle()`: the first column of the first row (or the
    /// whole row as an array with `entire_row`), `null`/`[]` without rows,
    /// `false` on error.
    pub fn query_single(
        &self,
        host: &mut dyn Host,
        sql: &[u8],
        entire_row: bool,
        used_ret: bool,
    ) -> Result<Value, PhpError> {
        let conn = self.db.check_init()?;
        if sql.is_empty() {
            return Ok(Value::Bool(false));
        }
        let mut cx = Cx::new(host);
        if !used_ret {
            self.exec_unused(&mut cx, &conn, sql);
            return cx.finish(Value::Bool(false));
        }
        let stmt = match ffi::enter(&mut cx, || conn.prepare(sql)) {
            Ok(s) => s,
            Err(code) => {
                let msg = format!("Unable to prepare statement: {}", lossy(&conn.errmsg()));
                sqlite_error(&mut cx, Some(&self.db.shared), code, &msg);
                return cx.finish(Value::Bool(false));
            }
        };
        let rc = ffi::enter(&mut cx, || stmt.step());
        let shared = Some(&*self.db.shared);
        let ret = match rc {
            SQLITE_ROW => {
                if entire_row {
                    let mut arr = ZArray::new();
                    for i in 0..stmt.data_count() {
                        let v = column_value(&mut cx, shared, &stmt, i);
                        let name = stmt.column_name(i).unwrap_or_default();
                        arr.make_mut().insert(ArrayKey::from_bytes(&name), v);
                    }
                    Value::Array(arr)
                } else {
                    column_value(&mut cx, shared, &stmt, 0)
                }
            }
            SQLITE_DONE => {
                if entire_row {
                    Value::Array(ZArray::new())
                } else {
                    Value::Null
                }
            }
            _ => {
                if !cx.has_exception() {
                    let msg = format!("Unable to execute statement: {}", lossy(&conn.errmsg()));
                    sqlite_error(&mut cx, shared, conn.errcode(), &msg);
                }
                Value::Bool(false)
            }
        };
        stmt.finalize();
        cx.finish(ret)
    }

    /// `SQLite3::createFunction()`. `callback` must already be validated as
    /// callable by the VM.
    pub fn create_function(
        &self,
        name: &[u8],
        callback: Value,
        argc: i64,
        flags: i64,
    ) -> Result<bool, PhpError> {
        let conn = self.db.check_init()?;
        if name.is_empty() {
            return Ok(false);
        }
        let f = UserFunction::Scalar(Box::new(Scalar {
            callable: callback,
            shared: Rc::clone(&self.db.shared),
        }));
        Ok(conn.create_function(name, argc, flags | i64::from(SQLITE_UTF8), Some(f)) == SQLITE_OK)
    }

    /// `SQLite3::createAggregate()`.
    pub fn create_aggregate(
        &self,
        name: &[u8],
        step: Value,
        finalize: Value,
        argc: i64,
    ) -> Result<bool, PhpError> {
        let conn = self.db.check_init()?;
        if name.is_empty() {
            return Ok(false);
        }
        let f = UserFunction::Aggregate(Box::new(Aggregate {
            step,
            fini: finalize,
            shared: Rc::clone(&self.db.shared),
        }));
        Ok(conn.create_function(name, argc, i64::from(SQLITE_UTF8), Some(f)) == SQLITE_OK)
    }

    /// `SQLite3::createCollation()`.
    pub fn create_collation(&self, name: &[u8], callback: Value) -> Result<bool, PhpError> {
        let conn = self.db.check_init()?;
        if name.is_empty() {
            return Ok(false);
        }
        let c = Collation {
            callable: callback,
            shared: Rc::clone(&self.db.shared),
        };
        Ok(conn.create_collation(name, Some(Box::new(c))) == SQLITE_OK)
    }

    /// `SQLite3::openBlob()`: `None` is `false` (after the "Unable to open
    /// blob" error). Wrap the result with [`BlobStream::into_stream`].
    pub fn open_blob(
        &self,
        host: &mut dyn Host,
        table: &[u8],
        column: &[u8],
        rowid: i64,
        dbname: &[u8],
        flags: i64,
    ) -> Result<Option<BlobStream>, PhpError> {
        let conn = self.db.check_init()?;
        let writable = flags & SQLITE3_OPEN_READWRITE != 0;
        if let Ok(blob) = conn.blob_open(dbname, table, column, rowid, writable) {
            Ok(Some(BlobStream::new(
                blob,
                flags,
                writable,
                BlobFlavor::Sqlite3,
            )))
        } else {
            let mut cx = Cx::new(host);
            let msg = format!("Unable to open blob: {}", lossy(&conn.errmsg()));
            sqlite_error(&mut cx, Some(&self.db.shared), conn.errcode(), &msg);
            cx.finish(None)
        }
    }

    /// `SQLite3::enableExceptions()`: returns the previous setting;
    /// disabling emits the deprecation.
    pub fn enable_exceptions(&self, host: &mut dyn Host, enable: bool) -> Result<bool, PhpError> {
        let old = self.db.shared.exception.get();
        let mut cx = Cx::new(host);
        if !enable {
            cx.docref(
                ErrorLevel::Deprecated,
                "Use of warnings for SQLite3 is deprecated",
            );
        }
        self.db.shared.exception.set(enable);
        cx.finish(old)
    }

    /// `SQLite3::setAuthorizer()` (`None` disables the user authorizer).
    pub fn set_authorizer(&self, callback: Option<Value>) -> Result<bool, PhpError> {
        self.db.check_init()?;
        *self.db.shared.authorizer.borrow_mut() = callback;
        Ok(true)
    }

    /// `SQLite3::backup()`.
    pub fn backup(
        &self,
        host: &mut dyn Host,
        destination: &Sqlite3,
        source_db: &[u8],
        destination_db: &[u8],
    ) -> Result<bool, PhpError> {
        let src = self.db.check_init()?;
        let dst = destination.db.check_init()?;
        let rc = src.backup_to(&dst, destination_db, source_db);
        if rc == SQLITE_OK {
            return Ok(true);
        }
        let mut cx = Cx::new(host);
        let shared = Some(&*self.db.shared);
        let msg = match rc {
            SQLITE_BUSY => "Backup failed: source database is busy".to_string(),
            SQLITE_LOCKED => "Backup failed: source database is locked".to_string(),
            _ => format!("Backup failed: {}", lossy(&src.errmsg())),
        };
        sqlite_error(&mut cx, shared, rc, &msg);
        cx.finish(false)
    }
}

/// `sqlite_value_to_zval()` with the out-of-memory error reported.
fn column_value(cx: &mut Cx<'_>, db: Option<&DbShared>, stmt: &RawStatement, col: i32) -> Value {
    match sqlite3_column_value(stmt, col) {
        ColumnValue::Value(v) => v,
        ColumnValue::OutOfMemory => {
            sqlite_error(
                cx,
                db,
                SQLITE_NOMEM,
                "Failed to retrieve column value due to out of memory",
            );
            Value::Null
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite3Stmt
// ---------------------------------------------------------------------------

/// `struct php_sqlite3_bound_param`.
#[derive(Debug, Clone)]
struct BoundParam {
    /// Hash key: the normalized name, or the number.
    key: BoundKey,
    param_number: i64,
    type_: i64,
    value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BoundKey {
    Num(i64),
    Name(Vec<u8>),
}

#[derive(Debug)]
struct StmtObject {
    db: Rc<DbObject>,
    stmt: Rc<RawStatement>,
    initialised: Cell<bool>,
    bound: RefCell<Vec<BoundParam>>,
}

impl StmtObject {
    /// `SQLITE3_CHECK_INITIALIZED(stmt_obj->db_obj, stmt_obj->initialised, SQLite3)`.
    fn check_init(&self) -> Result<(), PhpError> {
        if self.initialised.get() {
            Ok(())
        } else {
            Err(PhpError::error(NOT_INIT_DB))
        }
    }

    /// Both initialisation checks of most statement methods.
    fn check_stmt(&self) -> Result<(), PhpError> {
        self.check_init()?;
        if self.stmt.is_null() {
            return Err(PhpError::error(NOT_INIT_STMT));
        }
        Ok(())
    }

    fn shared(&self) -> &DbShared {
        &self.db.shared
    }

    fn bind_error(&self, cx: &mut Cx<'_>, rc: i32, num: i64) {
        if rc != SQLITE_OK {
            sqlite_error(
                cx,
                Some(self.shared()),
                rc,
                &format!("Unable to bind parameter number {num}"),
            );
        }
    }

    /// `php_sqlite3_bind_params()`; `false` is `FAILURE`.
    fn bind_params(&self, cx: &mut Cx<'_>) -> bool {
        let params = self.bound.borrow().clone();
        for p in params {
            let num = p.param_number;
            let parameter = p.value.deref();
            if parameter.is_null() {
                let rc = self.stmt.bind_null(num);
                self.bind_error(cx, rc, num);
                continue;
            }
            match i32::try_from(p.type_).unwrap_or(-1) {
                SQLITE_INTEGER => {
                    let r = parameter.to_long(cx.host());
                    let l = cx.check(r).unwrap_or(0);
                    self.write_back(&p.key, Value::Int(l));
                    let rc = self.stmt.bind_int(num, l);
                    self.bind_error(cx, rc, num);
                }
                SQLITE_FLOAT => {
                    let r = parameter.to_double(cx.host());
                    let d = cx.check(r).unwrap_or(0.0);
                    self.write_back(&p.key, Value::Float(d));
                    let rc = self.stmt.bind_double(num, d);
                    self.bind_error(cx, rc, num);
                }
                SQLITE_BLOB => {
                    let buffer = if let Value::Resource(_) = parameter {
                        if let Some(b) = cx.host().read_stream(&parameter) {
                            b
                        } else {
                            sqlite_error(
                                cx,
                                Some(self.shared()),
                                0,
                                &format!("Unable to read stream for parameter {num}"),
                            );
                            return false;
                        }
                    } else {
                        let r = parameter.to_zstr(cx.host());
                        cx.check(r).map(ZStr::into_bytes).unwrap_or_default()
                    };
                    let rc = self.stmt.bind_blob(num, &buffer);
                    self.bind_error(cx, rc, num);
                }
                SQLITE_TEXT => {
                    let r = parameter.to_zstr(cx.host());
                    let Some(s) = cx.check(r) else {
                        return false;
                    };
                    let rc = self.stmt.bind_text(num, s.as_bytes());
                    self.bind_error(cx, rc, num);
                }
                SQLITE_NULL => {
                    let rc = self.stmt.bind_null(num);
                    self.bind_error(cx, rc, num);
                }
                _ => {
                    sqlite_error(
                        cx,
                        Some(self.shared()),
                        0,
                        &format!("Unknown parameter type: {} for parameter {num}", p.type_),
                    );
                    return false;
                }
            }
        }
        true
    }

    /// `convert_to_long(parameter)` / `convert_to_double(parameter)` write
    /// the converted value back into the bound zval (and through a
    /// reference into the user's variable).
    fn write_back(&self, key: &BoundKey, v: Value) {
        let mut bound = self.bound.borrow_mut();
        if let Some(p) = bound.iter_mut().find(|p| &p.key == key) {
            set_slot(&mut p.value, v);
        }
    }
}

impl Drop for StmtObject {
    fn drop(&mut self) {
        // php_sqlite3_stmt_object_free_storage
        if self.initialised.get() {
            self.db.free_list_remove(self);
        }
    }
}

/// A `SQLite3Stmt` object.
#[derive(Debug, Clone)]
pub struct Sqlite3Stmt {
    obj: Rc<StmtObject>,
}

impl Sqlite3Stmt {
    /// `new SQLite3Stmt($db, $sql)` (the constructor is private in PHP but
    /// reachable through reflection): `None` is `false`.
    pub fn construct(
        host: &mut dyn Host,
        db: &Sqlite3,
        sql: &[u8],
    ) -> Result<Option<Self>, PhpError> {
        db.prepare(host, sql)
    }

    /// Whether two handles refer to the same statement object.
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        Rc::ptr_eq(&a.obj, &b.obj)
    }

    /// `SQLite3Stmt::paramCount()`.
    pub fn param_count(&self) -> Result<i64, PhpError> {
        self.obj.check_stmt()?;
        Ok(i64::from(self.obj.stmt.bind_parameter_count()))
    }

    /// `SQLite3Stmt::close()`.
    pub fn close(&self) -> Result<bool, PhpError> {
        self.obj.check_init()?;
        self.obj.db.free_list_remove(&self.obj);
        Ok(true)
    }

    /// `SQLite3Stmt::reset()`.
    pub fn reset(&self, host: &mut dyn Host) -> Result<bool, PhpError> {
        self.obj.check_stmt()?;
        if self.obj.stmt.reset() != SQLITE_OK {
            let mut cx = Cx::new(host);
            let msg = format!(
                "Unable to reset statement: {}",
                lossy(&self.obj.stmt.db_errmsg())
            );
            sqlite_error(
                &mut cx,
                Some(self.obj.shared()),
                self.obj.stmt.db_errcode(),
                &msg,
            );
            return cx.finish(false);
        }
        Ok(true)
    }

    /// `SQLite3Stmt::clear()`.
    pub fn clear(&self, host: &mut dyn Host) -> Result<bool, PhpError> {
        self.obj.check_stmt()?;
        if self.obj.stmt.clear_bindings() != SQLITE_OK {
            let mut cx = Cx::new(host);
            let msg = format!(
                "Unable to clear statement: {}",
                lossy(&self.obj.stmt.db_errmsg())
            );
            sqlite_error(
                &mut cx,
                Some(self.obj.shared()),
                self.obj.stmt.db_errcode(),
                &msg,
            );
            return cx.finish(false);
        }
        self.obj.bound.borrow_mut().clear();
        Ok(true)
    }

    /// `SQLite3Stmt::readOnly()`.
    pub fn read_only(&self) -> Result<bool, PhpError> {
        self.obj.check_stmt()?;
        Ok(self.obj.stmt.readonly())
    }

    /// `SQLite3Stmt::busy()`.
    pub fn busy(&self) -> Result<bool, PhpError> {
        self.obj.check_stmt()?;
        Ok(self.obj.stmt.busy())
    }

    /// `SQLite3Stmt::explain()`.
    pub fn explain(&self) -> Result<i64, PhpError> {
        self.obj.check_stmt()?;
        Ok(i64::from(self.obj.stmt.isexplain()))
    }

    /// `SQLite3Stmt::setExplain()`.
    pub fn set_explain(&self, mode: i64) -> Result<bool, PhpError> {
        if !(0..=2).contains(&mode) {
            return Err(PhpError::new(
                ExceptionClass::ValueError,
                "SQLite3Stmt::setExplain(): Argument #1 ($mode) must be one of the SQLite3Stmt::EXPLAIN_MODE_* constants",
                0,
            ));
        }
        self.obj.check_stmt()?;
        #[allow(clippy::cast_possible_truncation)]
        Ok(self.obj.stmt.explain(mode as i32) == SQLITE_OK)
    }

    /// `SQLite3Stmt::bindParam()` / `bindValue()`. For `bindParam()` pass a
    /// [`Value::Reference`]; `type_` is `None` when the argument was
    /// omitted (the type is then inferred from the value; a reference always
    /// infers `SQLITE3_TEXT`).
    pub fn bind(
        &self,
        param: ParamKey<'_>,
        value: Value,
        type_: Option<i64>,
    ) -> Result<bool, PhpError> {
        self.obj.check_stmt()?;
        let type_ = type_.unwrap_or_else(|| match &value {
            Value::Int(_) | Value::Bool(_) => i64::from(SQLITE_INTEGER),
            Value::Float(_) => i64::from(SQLITE_FLOAT),
            Value::Null => i64::from(SQLITE_NULL),
            _ => i64::from(SQLITE_TEXT),
        });
        let (key, number) = match param {
            ParamKey::Name(name) => {
                let name = if name.first() == Some(&b':') || name.first() == Some(&b'@') {
                    name.to_vec()
                } else {
                    let mut n = Vec::with_capacity(name.len() + 1);
                    n.push(b':');
                    n.extend_from_slice(name);
                    n
                };
                let number = i64::from(self.obj.stmt.bind_parameter_index(&name));
                (BoundKey::Name(name), number)
            }
            ParamKey::Number(n) => (BoundKey::Num(n), n),
        };
        if number < 1 {
            return Ok(false);
        }
        let mut bound = self.obj.bound.borrow_mut();
        bound.retain(|p| p.key != BoundKey::Num(number));
        let entry = BoundParam {
            key: key.clone(),
            param_number: number,
            type_,
            value,
        };
        if let Some(p) = bound.iter_mut().find(|p| p.key == key) {
            *p = entry;
        } else {
            bound.push(entry);
        }
        Ok(true)
    }

    /// `SQLite3Stmt::bindParam()` (the variable as a [`Value::Reference`]).
    pub fn bind_param(
        &self,
        param: ParamKey<'_>,
        var: Value,
        type_: Option<i64>,
    ) -> Result<bool, PhpError> {
        self.bind(param, var, type_)
    }

    /// `SQLite3Stmt::bindValue()`.
    pub fn bind_value(
        &self,
        param: ParamKey<'_>,
        value: &Value,
        type_: Option<i64>,
    ) -> Result<bool, PhpError> {
        self.bind(param, value.deref(), type_)
    }

    /// `SQLite3Stmt::getSQL()`: `None` is `false`.
    pub fn get_sql(&self, host: &mut dyn Host, expanded: bool) -> Result<Option<ZStr>, PhpError> {
        self.obj.check_stmt()?;
        let mut cx = Cx::new(host);
        if !self.obj.bind_params(&mut cx) || cx.has_exception() {
            return cx.finish(None);
        }
        let sql = if expanded {
            self.obj.stmt.expanded_sql()
        } else {
            self.obj.stmt.sql()
        };
        cx.finish(Some(ZStr::from(sql)))
    }

    /// `SQLite3Stmt::execute()`: `None` is `false`.
    pub fn execute(&self, host: &mut dyn Host) -> Result<Option<Sqlite3Result>, PhpError> {
        self.obj.check_init()?;
        let stmt = Rc::clone(&self.obj.stmt);
        let mut cx = Cx::new(host);
        // Always reset before execution (bug #77051).
        stmt.reset();
        if !self.obj.bind_params(&mut cx) || cx.has_exception() {
            return cx.finish(None);
        }
        let rc = ffi::enter(&mut cx, || stmt.step());
        match rc {
            SQLITE_ROW | SQLITE_DONE => {
                stmt.reset();
                cx.finish(Some(Sqlite3Result::new(Rc::clone(&self.obj), true)))
            }
            _ => {
                if rc == SQLITE_ERROR {
                    stmt.reset();
                }
                if !cx.has_exception() {
                    let msg = format!("Unable to execute statement: {}", lossy(&stmt.db_errmsg()));
                    sqlite_error(&mut cx, Some(self.obj.shared()), stmt.db_errcode(), &msg);
                }
                cx.finish(None)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite3Result
// ---------------------------------------------------------------------------

/// A `SQLite3Result` object.
#[derive(Debug)]
pub struct Sqlite3Result {
    stmt: Rc<StmtObject>,
    is_prepared_statement: bool,
    column_count: Cell<i32>,
    column_names: RefCell<Option<Vec<ZStr>>>,
}

impl Sqlite3Result {
    fn new(stmt: Rc<StmtObject>, is_prepared_statement: bool) -> Self {
        Sqlite3Result {
            stmt,
            is_prepared_statement,
            column_count: Cell::new(-1),
            column_names: RefCell::new(None),
        }
    }

    /// `SQLITE3_CHECK_INITIALIZED(result_obj->db_obj, result_obj->stmt_obj->initialised, SQLite3Result)`.
    fn check(&self) -> Result<&RawStatement, PhpError> {
        if self.stmt.initialised.get() {
            Ok(&self.stmt.stmt)
        } else {
            Err(PhpError::error(NOT_INIT_RESULT))
        }
    }

    fn shared(&self) -> &DbShared {
        &self.stmt.db.shared
    }

    /// `SQLite3Result::numColumns()`.
    pub fn num_columns(&self) -> Result<i64, PhpError> {
        Ok(i64::from(self.check()?.column_count()))
    }

    /// `SQLite3Result::columnName()`: `None` is `false`.
    pub fn column_name(&self, column: i64) -> Result<Option<ZStr>, PhpError> {
        let stmt = self.check()?;
        #[allow(clippy::cast_possible_truncation)]
        Ok(stmt.column_name(column as i32).map(ZStr::from))
    }

    /// `SQLite3Result::columnType()`: `None` is `false` (no current row).
    pub fn column_type(&self, column: i64) -> Result<Option<i64>, PhpError> {
        let stmt = self.check()?;
        if stmt.data_count() == 0 {
            return Ok(None);
        }
        #[allow(clippy::cast_possible_truncation)]
        Ok(Some(i64::from(stmt.column_type(column as i32))))
    }

    fn fill_column_names(&self, stmt: &RawStatement, n: i32) {
        let names = (0..n)
            .map(|i| ZStr::from(stmt.column_name(i).unwrap_or_default()))
            .collect();
        *self.column_names.borrow_mut() = Some(names);
    }

    fn clear_column_names(&self) {
        *self.column_names.borrow_mut() = None;
        self.column_count.set(-1);
    }

    /// `php_sqlite3_fetch_one()`.
    fn fetch_one(&self, cx: &mut Cx<'_>, stmt: &RawStatement, n_cols: i32, mode: i64) -> ZArray {
        let mut row = ZArray::new();
        let names = self.column_names.borrow().clone();
        for i in 0..n_cols {
            let data = column_value(cx, Some(self.shared()), stmt, i);
            if mode & SQLITE3_NUM != 0 {
                row.make_mut()
                    .insert(ArrayKey::Int(i64::from(i)), data.clone());
            }
            if mode & SQLITE3_ASSOC != 0 {
                let name = names
                    .as_ref()
                    .and_then(|n| n.get(usize::try_from(i).unwrap_or(usize::MAX)).cloned())
                    .unwrap_or_else(ZStr::empty);
                row.make_mut().insert(ArrayKey::from_zstr(&name), data);
            }
        }
        row
    }

    /// `SQLite3Result::fetchArray()`: the next row, or `false`.
    pub fn fetch_array(&self, host: &mut dyn Host, mode: i64) -> Result<Value, PhpError> {
        self.check()?;
        let stmt = Rc::clone(&self.stmt.stmt);
        let mut cx = Cx::new(host);
        let rc = ffi::enter(&mut cx, || stmt.step());
        match rc {
            SQLITE_ROW => {
                if self.column_count.get() == -1 {
                    self.column_count.set(stmt.column_count());
                }
                let n_cols = self.column_count.get();
                if mode & SQLITE3_ASSOC != 0 && self.column_names.borrow().is_none() {
                    self.fill_column_names(&stmt, n_cols);
                }
                let row = self.fetch_one(&mut cx, &stmt, n_cols, mode);
                cx.finish(Value::Array(row))
            }
            SQLITE_DONE => cx.finish(Value::Bool(false)),
            _ => {
                let msg = format!("Unable to execute statement: {}", lossy(&stmt.db_errmsg()));
                sqlite_error(&mut cx, Some(self.shared()), stmt.db_errcode(), &msg);
                cx.finish(Value::Bool(false))
            }
        }
    }

    /// `SQLite3Result::fetchAll()`: all remaining rows, or `false`.
    pub fn fetch_all(&self, host: &mut dyn Host, mode: i64) -> Result<Value, PhpError> {
        self.check()?;
        let stmt = Rc::clone(&self.stmt.stmt);
        let nb_cols = stmt.column_count();
        if mode & SQLITE3_ASSOC != 0 && self.column_names.borrow().is_none() {
            self.fill_column_names(&stmt, nb_cols);
        }
        self.column_count.set(nb_cols);
        let mut cx = Cx::new(host);
        let mut all = ZArray::new();
        loop {
            let rc = ffi::enter(&mut cx, || stmt.step());
            match rc {
                SQLITE_ROW => {
                    let row = self.fetch_one(&mut cx, &stmt, nb_cols, mode);
                    // At `memory_limit` the loop ends and the VM raises the
                    // pending fatal when the call returns.
                    let stored = php_memlimit::poll()
                        .and_then(|()| all.make_mut().try_push(Value::Array(row)).map(drop));
                    if stored.is_err() {
                        break;
                    }
                }
                SQLITE_DONE => break,
                _ => {
                    if !cx.has_exception() {
                        let msg =
                            format!("Unable to execute statement: {}", lossy(&stmt.db_errmsg()));
                        sqlite_error(&mut cx, Some(self.shared()), stmt.db_errcode(), &msg);
                    }
                    return cx.finish(Value::Bool(false));
                }
            }
        }
        cx.finish(Value::Array(all))
    }

    /// `SQLite3Result::reset()`.
    pub fn reset(&self, host: &mut dyn Host) -> Result<bool, PhpError> {
        let stmt = self.check()?;
        self.clear_column_names();
        if stmt.reset() != SQLITE_OK {
            let mut cx = Cx::new(host);
            let msg = format!(
                "Statement reset reported an error: {}",
                lossy(&stmt.db_errmsg())
            );
            sqlite_error(&mut cx, Some(self.shared()), stmt.db_errcode(), &msg);
            return cx.finish(false);
        }
        Ok(true)
    }

    /// `SQLite3Result::finalize()`.
    pub fn finalize(&self, host: &mut dyn Host) -> Result<bool, PhpError> {
        let stmt = self.check()?;
        self.clear_column_names();
        if !self.is_prepared_statement {
            self.stmt.db.free_list_remove(&self.stmt);
        } else if stmt.reset() != SQLITE_OK {
            let mut cx = Cx::new(host);
            let msg = format!(
                "Statement reset reported an error: {}",
                lossy(&stmt.db_errmsg())
            );
            sqlite_error(&mut cx, Some(self.shared()), stmt.db_errcode(), &msg);
            return cx.finish(true);
        }
        Ok(true)
    }
}

impl Drop for Sqlite3Result {
    fn drop(&mut self) {
        if self.stmt.initialised.get() {
            self.stmt.stmt.reset();
        }
    }
}
