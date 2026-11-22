// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Prepared statements (ref: `mysqlnd_ps.c`, `mysqlnd_ps_codec.c`):
//! prepare, parameter binding and binary encoding, execute, buffered /
//! unbuffered / cursor fetching, long data, reset and close.

use std::cell::{RefCell, RefMut};
use std::rc::Rc;

use php_runtime::error::{RuntimeContext, RuntimeError};
use php_runtime::{Value, ZStr};

use crate::conn::{ConnState, Connection, Header, PacketKind, QueryType, UpsertStatus};
use crate::consts::{
    Command, CR_COMMANDS_OUT_OF_SYNC, CR_INVALID_BUFFER_USE, CR_INVALID_PARAMETER_NO,
    CR_NOT_IMPLEMENTED, CR_NO_PREPARE_STMT, CR_PARAMS_NOT_BOUND, CR_UNKNOWN_ERROR,
    CURSOR_TYPE_NO_CURSOR, CURSOR_TYPE_READ_ONLY, MSG_NOT_BOUND_AS_BLOB, MSG_OUT_OF_SYNC,
    MSG_STMT_NOT_PREPARED, MYSQL_TYPE_DOUBLE, MYSQL_TYPE_LONG, MYSQL_TYPE_LONGLONG,
    MYSQL_TYPE_LONG_BLOB, MYSQL_TYPE_NULL, MYSQL_TYPE_TINY, MYSQL_TYPE_VAR_STRING,
    SERVER_MORE_RESULTS_EXISTS, SERVER_PS_OUT_PARAMS, SERVER_STATUS_CURSOR_EXISTS,
    STMT_ATTR_CURSOR_TYPE, STMT_ATTR_UPDATE_MAX_LENGTH,
};
use crate::error::{Error, ErrorInfo, Result};
use crate::protocol::{self, RowPacket};
use crate::result::{Metadata, ResultSet};
use crate::stats::{inc_global, Stat};
use crate::wire::put_lenenc;

/// `enum mysqlnd_stmt_state`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StmtState {
    /// Allocated.
    Initted = 0,
    /// Prepared.
    Prepared = 1,
    /// Executed (no result set).
    Executed = 2,
    /// Executed; a result set waits for store/use.
    WaitingUseOrStore = 3,
    /// `store_result` / `use_result` was called.
    UseOrStoreCalled = 4,
    /// Rows are being fetched.
    UserFetching = 5,
}

/// One bound parameter (`MYSQLND_PARAM_BIND`).
#[derive(Debug, Clone)]
pub struct ParamBind {
    /// `MYSQL_TYPE_*` (mysqli: `i` = LONGLONG, `d` = DOUBLE, `s` =
    /// `VAR_STRING`, `b` = `LONG_BLOB`).
    pub type_: u8,
    /// The bound value; a [`Value::Reference`] is read at execute time.
    /// `None` = not bound (`IS_UNDEF`).
    pub value: Option<Value>,
    blob_used: bool,
}

impl ParamBind {
    /// A bound parameter.
    #[must_use]
    pub fn new(type_: u8, value: Value) -> Self {
        Self {
            type_,
            value: Some(value),
            blob_used: false,
        }
    }
}

/// Maps a `mysqli_stmt_bind_param` type character to a wire type.
#[must_use]
pub fn param_type_from_char(c: u8) -> Option<u8> {
    match c {
        b'd' => Some(MYSQL_TYPE_DOUBLE),
        b'i' => Some(MYSQL_TYPE_LONGLONG),
        b'b' => Some(MYSQL_TYPE_LONG_BLOB),
        b's' => Some(MYSQL_TYPE_VAR_STRING),
        _ => None,
    }
}

/// Failure of [`Statement::execute`].
#[derive(Debug)]
pub enum ExecuteError {
    /// A `MySQL` / client error (also recorded on the statement).
    Mysql(Error),
    /// Converting a parameter raised a PHP exception.
    Runtime(RuntimeError),
}

impl From<Error> for ExecuteError {
    fn from(e: Error) -> Self {
        ExecuteError::Mysql(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RsetHandler {
    Store,
    Use,
}

/// `MYSQLND_STMT_DATA`
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct StmtData {
    conn: Option<Connection>,
    stmt_id: u32,
    state: StmtState,
    flags: u64,
    update_max_length: bool,
    field_count: u32,
    param_count: u32,
    upsert: UpsertStatus,
    error_info: ErrorInfo,
    meta: Option<Rc<Metadata>>,
    result: Option<ResultSet>,
    params: Option<Vec<ParamBind>>,
    send_types_to_server: bool,
    result_bound: bool,
    execute_count: u64,
    cursor_exists: bool,
    handler: RsetHandler,
    closed: bool,
}

/// A prepared statement handle (`MYSQLND_STMT`).
#[derive(Debug, Clone)]
pub struct Statement(Rc<RefCell<StmtData>>);

impl Drop for StmtData {
    fn drop(&mut self) {
        if !self.closed {
            self.dtor(true);
        }
    }
}

fn copy_conn_error(s: &mut StmtData) {
    if let Some(c) = &s.conn {
        let e = c.data().error_info.clone();
        s.error_info.copy_from(&e);
    }
}

impl StmtData {
    fn conn(&self) -> Option<Connection> {
        self.conn.clone()
    }

    fn set_error(&mut self, errno: u32, msg: &str) {
        self.error_info.set_client(errno, msg);
    }

    /// `mysqlnd_stmt_check_state`
    fn check_state(&self, conn: &Connection) -> bool {
        if self.state != StmtState::WaitingUseOrStore {
            return false;
        }
        let st = conn.data().state;
        if self.cursor_exists {
            st == ConnState::Ready
        } else {
            st == ConnState::FetchingData
        }
    }

    fn send_cursor_fetch(&mut self, conn: &Connection, max_rows: u32) -> bool {
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(&self.stmt_id.to_le_bytes());
        buf.extend_from_slice(&max_rows.to_le_bytes());
        let ok = conn.data().send_command(Command::StmtFetch, &buf, false);
        if !ok {
            copy_conn_error(self);
        }
        ok
    }

    fn run_handler(&mut self) {
        match self.handler {
            RsetHandler::Store => {
                let _ = self.store_result();
            }
            RsetHandler::Use => {
                let _ = self.use_result();
            }
        }
    }

    /// `mysqlnd_stmt::store_result`
    fn store_result(&mut self) -> bool {
        let Some(conn) = self.conn() else {
            return false;
        };
        let Some(meta) = self.meta.clone() else {
            return false;
        };
        if self.field_count == 0 {
            return false;
        }
        if !self.check_state(&conn) {
            self.set_error(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
            return false;
        }
        self.handler = RsetHandler::Store;
        self.error_info.reset();
        conn.data().error_info.reset();
        conn.data().inc(Stat::PsBufferedSets, 1);
        if self.cursor_exists && !self.send_cursor_fetch(&conn, u32::MAX) {
            return false;
        }
        let rows = conn.data().fetch_all_rows(true);
        match rows {
            Ok(rows) => {
                self.upsert.affected_rows = rows.len() as u64;
                let mut rs = ResultSet::buffered(Some(conn), meta, rows, true);
                rs.stmt_owned = true;
                self.result = Some(rs);
                self.state = StmtState::UseOrStoreCalled;
                true
            }
            Err(e) => {
                conn.data().error_info.set(e.errno, &e.sqlstate, &e.error);
                self.error_info.set(e.errno, &e.sqlstate, &e.error);
                self.result = None;
                self.state = StmtState::Prepared;
                false
            }
        }
    }

    /// `mysqlnd_stmt::use_result`
    fn use_result(&mut self) -> bool {
        let Some(conn) = self.conn() else {
            return false;
        };
        let Some(meta) = self.meta.clone() else {
            return false;
        };
        if self.field_count == 0 || !self.check_state(&conn) {
            conn.data()
                .error_info
                .set_client(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
            return false;
        }
        self.error_info.reset();
        conn.data().inc(Stat::PsUnbufferedSets, 1);
        let mut rs = ResultSet::unbuffered(conn, meta, Some(true));
        rs.stmt_owned = true;
        if self.cursor_exists {
            rs.cursor_stmt_id = Some(self.stmt_id);
        }
        self.result = Some(rs);
        self.state = StmtState::UseOrStoreCalled;
        true
    }

    fn more_results(&self) -> bool {
        self.conn
            .as_ref()
            .is_some_and(|c| c.data().upsert.server_status & SERVER_MORE_RESULTS_EXISTS != 0)
    }

    /// `mysqlnd_stmt::next_result`
    fn next_result(&mut self) -> bool {
        let Some(conn) = self.conn() else {
            return false;
        };
        if self.meta.is_none() && self.result.is_none() {
            return false;
        }
        {
            let c = conn.data();
            if c.state != ConnState::NextResultPending
                || c.upsert.server_status & SERVER_MORE_RESULTS_EXISTS == 0
            {
                return false;
            }
        }
        self.free_stmt_result();
        self.parse_execute_response()
    }

    /// `free_stmt_result`
    fn free_stmt_result(&mut self) {
        if let Some(mut r) = self.result.take() {
            r.stmt_owned = false;
            r.free(true);
        } else if self.meta.is_some() {
            if let Some(c) = &self.conn {
                c.data().inc(Stat::FreeResultImplicit, 1);
            }
        }
        self.meta = None;
        self.error_info.list.clear();
    }

    /// `mysqlnd_stmt::flush`
    fn flush(&mut self) {
        if self.stmt_id == 0 {
            return;
        }
        loop {
            if self.state == StmtState::WaitingUseOrStore {
                self.run_handler();
                self.state = StmtState::UserFetching;
            }
            if let Some(r) = &mut self.result {
                if r.cursor_stmt_id.is_none() {
                    r.release_buffers_keep();
                }
            }
            if !(self.more_results() && self.next_result()) {
                break;
            }
        }
    }

    /// `mysqlnd_stmt_execute_parse_response`
    fn parse_execute_response(&mut self) -> bool {
        let Some(conn) = self.conn() else {
            return false;
        };
        conn.data().state = ConnState::QuerySent;
        let header = conn.data().read_result_set_header(true);
        let mut ret = match header {
            Err(()) => {
                copy_conn_error(self);
                let affected = conn.data().upsert.affected_rows;
                self.upsert.reset();
                self.upsert.affected_rows = affected;
                self.state = StmtState::Prepared;
                self.send_types_to_server = true;
                return false;
            }
            Ok(h) => h,
        };
        self.error_info.reset();
        {
            let mut c = conn.data();
            c.error_info.reset();
            self.upsert.warning_count = c.upsert.warning_count;
            self.upsert.affected_rows = c.upsert.affected_rows;
            self.upsert.server_status = c.upsert.server_status;
            self.upsert.last_insert_id = c.upsert.last_insert_id;
        }
        self.state = StmtState::Executed;
        let (lqt, fc) = {
            let c = conn.data();
            (c.last_query_type, c.field_count)
        };
        if lqt == QueryType::Upsert || lqt == QueryType::LoadLocal {
            self.field_count = fc;
            return true;
        }
        if let Header::ResultSet(meta) = std::mem::replace(&mut ret, Header::NoResult) {
            self.meta = Some(meta);
        }
        self.result = None;
        self.field_count = fc;
        if self.field_count > 0 {
            self.state = StmtState::WaitingUseOrStore;
            if self.flags & CURSOR_TYPE_READ_ONLY != 0 {
                if self.upsert.server_status & SERVER_STATUS_CURSOR_EXISTS != 0 {
                    self.cursor_exists = true;
                    conn.data().state = ConnState::Ready;
                    self.handler = RsetHandler::Use;
                } else {
                    self.handler = RsetHandler::Store;
                }
            } else {
                self.handler = RsetHandler::Use;
            }
        }
        if self.upsert.server_status & SERVER_PS_OUT_PARAMS != 0 {
            self.free_stmt_content();
            return self.parse_execute_response();
        }
        true
    }

    fn free_stmt_content(&mut self) {
        self.params = None;
        self.free_stmt_result();
    }

    /// `mysqlnd_stmt::close_on_server`
    fn close_on_server(&mut self, implicit: bool) -> bool {
        let Some(conn) = self.conn() else {
            return false;
        };
        self.error_info.reset();
        conn.data().error_info.reset();
        loop {
            if self.state == StmtState::WaitingUseOrStore {
                self.run_handler();
                self.state = StmtState::UserFetching;
            }
            if let Some(r) = &mut self.result {
                if r.cursor_stmt_id.is_none() {
                    r.release_buffers_keep();
                }
            }
            if !(self.more_results() && self.next_result()) {
                break;
            }
        }
        let mut ret = true;
        if self.stmt_id != 0 {
            inc_global(
                if implicit {
                    Stat::FreeResultImplicit
                } else {
                    Stat::FreeResultExplicit
                },
                1,
            );
            let ready = conn.data().state == ConnState::Ready;
            if ready {
                ret = conn.data().send_command(
                    Command::StmtClose,
                    &self.stmt_id.to_le_bytes(),
                    false,
                );
            }
        }
        match self.execute_count {
            0 => conn.data().inc(Stat::PsPreparedNeverExecuted, 1),
            1 => conn.data().inc(Stat::PsPreparedOnceUsed, 1),
            _ => {}
        }
        self.free_stmt_content();
        self.conn = None;
        ret
    }

    /// `mysqlnd_stmt::dtor`
    fn dtor(&mut self, implicit: bool) -> bool {
        inc_global(
            if implicit {
                Stat::StmtCloseImplicit
            } else {
                Stat::StmtCloseExplicit
            },
            1,
        );
        self.closed = true;
        self.close_on_server(implicit)
    }

    /// `mysqlnd_stmt_execute_store_params` and friends: builds the
    /// `COM_STMT_EXECUTE` payload.
    #[allow(clippy::too_many_lines)]
    fn generate_request(
        &mut self,
        ctx: &mut dyn RuntimeContext,
    ) -> std::result::Result<Vec<u8>, RuntimeError> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&self.stmt_id.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        out.push(self.flags as u8);
        out.extend_from_slice(&1u32.to_le_bytes());
        if self.param_count == 0 {
            return Ok(out);
        }
        let n = self.param_count as usize;
        let params = self.params.clone().unwrap_or_default();
        let null_offset = out.len();
        out.resize(out.len() + n.div_ceil(8), 0);
        // the values the request is built from (`copies`, or the dereferenced bound value)
        let mut data: Vec<Value> = params
            .iter()
            .map(|p| match &p.value {
                Some(Value::Reference(r)) => r.get(),
                Some(v) => v.clone(),
                None => Value::Null,
            })
            .collect();
        // 1. prepare_param_types: long types that don't fit become strings
        let mut resend = false;
        for (i, p) in params.iter().enumerate() {
            let v = &mut data[i];
            if !matches!(v, Value::Null)
                && matches!(
                    p.type_,
                    MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_TINY
                )
                && !matches!(v, Value::Int(_))
            {
                let d = v.to_double(ctx)?;
                #[allow(clippy::cast_precision_loss)]
                if d >= i64::MAX as f64 || d < i64::MIN as f64 {
                    self.send_types_to_server = true;
                    resend = true;
                    *v = Value::Str(v.to_zstr(ctx)?);
                } else {
                    *v = Value::Int(v.to_long(ctx)?);
                }
            }
        }
        out.push(u8::from(self.send_types_to_server));
        if self.send_types_to_server {
            for (i, p) in params.iter().enumerate() {
                let mut t = p.type_;
                if t == MYSQL_TYPE_LONG {
                    t = MYSQL_TYPE_LONGLONG;
                }
                if !matches!(data[i], Value::Null)
                    && matches!(t, MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG)
                    && matches!(data[i], Value::Str(_))
                {
                    t = MYSQL_TYPE_VAR_STRING;
                }
                out.extend_from_slice(&u16::from(t).to_le_bytes());
            }
        }
        self.send_types_to_server = resend;
        // 2. calculate_param_values_size: conversions
        for (i, p) in params.iter().enumerate() {
            if p.type_ != MYSQL_TYPE_LONG_BLOB && matches!(data[i], Value::Null) {
                continue;
            }
            match p.type_ {
                MYSQL_TYPE_LONGLONG | MYSQL_TYPE_LONG if !matches!(data[i], Value::Str(_)) => {
                    data[i] = Value::Int(data[i].to_long(ctx)?);
                }
                MYSQL_TYPE_VAR_STRING if !matches!(data[i], Value::Str(_)) => {
                    data[i] = Value::Str(data[i].to_zstr(ctx)?);
                }
                _ => {}
            }
        }
        // 3. store_param_values
        for (i, p) in params.iter().enumerate() {
            let unbound = p.value.is_none();
            if !unbound && matches!(data[i], Value::Null) {
                out[null_offset + i / 8] |= 1 << (i & 7);
                continue;
            }
            match p.type_ {
                MYSQL_TYPE_DOUBLE => {
                    let d = data[i].to_double(ctx)?;
                    out.extend_from_slice(&d.to_le_bytes());
                }
                MYSQL_TYPE_LONGLONG | MYSQL_TYPE_LONG | MYSQL_TYPE_TINY => {
                    if let Value::Str(s) = &data[i] {
                        put_lenenc(&mut out, s.len() as u64);
                        out.extend_from_slice(s.as_bytes());
                    } else {
                        let l = match data[i] {
                            Value::Int(l) => l,
                            ref v => v.to_long(ctx)?,
                        };
                        match p.type_ {
                            MYSQL_TYPE_LONGLONG => out.extend_from_slice(&l.to_le_bytes()),
                            #[allow(clippy::cast_possible_truncation)]
                            MYSQL_TYPE_LONG => out.extend_from_slice(&(l as i32).to_le_bytes()),
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            _ => out.push(l as u8),
                        }
                    }
                }
                MYSQL_TYPE_LONG_BLOB => {
                    if let Some(ps) = &mut self.params {
                        if ps[i].blob_used {
                            ps[i].blob_used = false;
                        } else {
                            put_lenenc(&mut out, 0);
                        }
                    }
                }
                MYSQL_TYPE_VAR_STRING => {
                    let s: ZStr = match &data[i] {
                        Value::Str(s) => s.clone(),
                        v => v.to_zstr(ctx)?,
                    };
                    put_lenenc(&mut out, s.len() as u64);
                    out.extend_from_slice(s.as_bytes());
                }
                _ => out[null_offset + i / 8] |= 1 << (i & 7),
            }
        }
        Ok(out)
    }
}

impl ResultSet {
    /// Drains unread rows but keeps the set (and its metadata) usable.
    pub(crate) fn release_buffers_keep(&mut self) {
        self.skip_result();
    }
}

impl Statement {
    pub(crate) fn new(conn: Connection) -> Self {
        Statement(Rc::new(RefCell::new(StmtData {
            conn: Some(conn),
            stmt_id: 0,
            state: StmtState::Initted,
            flags: CURSOR_TYPE_NO_CURSOR,
            update_max_length: false,
            field_count: 0,
            param_count: 0,
            upsert: UpsertStatus::default(),
            error_info: ErrorInfo::default(),
            meta: None,
            result: None,
            params: None,
            send_types_to_server: false,
            result_bound: false,
            execute_count: 0,
            cursor_exists: false,
            handler: RsetHandler::Use,
            closed: false,
        })))
    }

    fn data(&self) -> RefMut<'_, StmtData> {
        self.0.borrow_mut()
    }

    fn fail<T>(&self) -> Result<T> {
        Err(self.data().error_info.to_error())
    }

    /// The owning connection (`None` after close).
    #[must_use]
    pub fn connection(&self) -> Option<Connection> {
        self.data().conn.clone()
    }

    /// `mysqlnd_stmt::prepare`
    pub fn prepare(&self, query: &[u8]) -> Result<()> {
        let ok = self.prepare_inner(query);
        if ok {
            Ok(())
        } else {
            self.fail()
        }
    }

    fn prepare_inner(&self, query: &[u8]) -> bool {
        let mut s = self.data();
        let Some(conn) = s.conn() else { return false };
        s.upsert.set_affected_rows_to_error();
        conn.data().upsert.set_affected_rows_to_error();
        s.error_info.reset();
        conn.data().error_info.reset();
        if s.state > StmtState::Initted {
            // destroy the previous statement and start afresh
            s.dtor(true);
            *s = StmtData {
                conn: Some(conn.clone()),
                stmt_id: 0,
                state: StmtState::Initted,
                flags: s.flags,
                update_max_length: s.update_max_length,
                field_count: 0,
                param_count: 0,
                upsert: UpsertStatus {
                    affected_rows: u64::MAX,
                    ..UpsertStatus::default()
                },
                error_info: ErrorInfo::default(),
                meta: None,
                result: None,
                params: None,
                send_types_to_server: false,
                result_bound: false,
                execute_count: 0,
                cursor_exists: false,
                handler: RsetHandler::Use,
                closed: false,
            };
        }
        if !conn.data().send_command(Command::StmtPrepare, query, false) {
            copy_conn_error(&mut s);
            return false;
        }
        // mysqlnd_stmt_read_prepare_response
        let resp = conn
            .data()
            .read_packet(PacketKind::Prepare)
            .and_then(|b| protocol::parse_prepare_response(&b));
        let resp = match resp {
            None => return false,
            Some(Err(e)) => {
                e.apply(&mut s.error_info);
                e.apply(&mut conn.data().error_info);
                return false;
            }
            Some(Ok(r)) => r,
        };
        s.stmt_id = resp.stmt_id;
        conn.data().upsert.warning_count = u32::from(resp.warning_count);
        s.upsert.affected_rows = 0;
        s.field_count = u32::from(resp.field_count);
        conn.data().field_count = s.field_count;
        s.param_count = u32::from(resp.param_count);
        if s.param_count > 0 {
            for _ in 0..s.param_count {
                if conn.data().read_packet(PacketKind::Field).is_none() {
                    return false;
                }
            }
            if !Self::prepare_read_eof(&mut s, &conn) {
                return false;
            }
        }
        if s.field_count > 0 {
            let meta = conn.data().read_metadata(s.field_count);
            let Ok(meta) = meta else { return false };
            s.meta = Some(Rc::new(meta));
            if !Self::prepare_read_eof(&mut s, &conn) {
                return false;
            }
        }
        s.state = StmtState::Prepared;
        true
    }

    fn prepare_read_eof(s: &mut StmtData, conn: &Connection) -> bool {
        match conn
            .data()
            .read_packet(PacketKind::Eof)
            .and_then(|b| protocol::parse_eof(&b))
        {
            Some(Ok(e)) => {
                s.upsert.server_status = e.server_status;
                s.upsert.warning_count = u32::from(e.warning_count);
                s.state = StmtState::Prepared;
                true
            }
            Some(Err(_)) => {
                s.state = StmtState::Prepared;
                true
            }
            None => {
                s.meta = None;
                s.state = StmtState::Initted;
                false
            }
        }
    }

    /// `mysqlnd_stmt::bind_parameters`: binds every parameter at once.
    pub fn bind_params(&self, params: Vec<ParamBind>) -> Result<()> {
        let mut s = self.data();
        if s.state < StmtState::Prepared {
            s.set_error(CR_NO_PREPARE_STMT, MSG_STMT_NOT_PREPARED);
            drop(s);
            return self.fail();
        }
        s.error_info.reset();
        if let Some(c) = &s.conn {
            c.data().error_info.reset();
        }
        if s.param_count > 0 {
            let mut params = params;
            params.resize(
                s.param_count as usize,
                ParamBind {
                    type_: MYSQL_TYPE_VAR_STRING,
                    value: None,
                    blob_used: false,
                },
            );
            for p in &mut params {
                p.blob_used = false;
            }
            s.params = Some(params);
            s.send_types_to_server = true;
        }
        Ok(())
    }

    /// `mysqlnd_stmt::bind_one_parameter`
    pub fn bind_one_param(&self, param_no: u32, value: Value, type_: u8) -> Result<()> {
        let mut s = self.data();
        if s.state < StmtState::Prepared {
            s.set_error(CR_NO_PREPARE_STMT, MSG_STMT_NOT_PREPARED);
            drop(s);
            return self.fail();
        }
        if param_no >= s.param_count {
            s.set_error(CR_INVALID_PARAMETER_NO, "Invalid parameter number");
            drop(s);
            return self.fail();
        }
        s.error_info.reset();
        if let Some(c) = &s.conn {
            c.data().error_info.reset();
        }
        let n = s.param_count as usize;
        let params = s.params.get_or_insert_with(|| {
            vec![
                ParamBind {
                    type_: MYSQL_TYPE_NULL,
                    value: None,
                    blob_used: false
                };
                n
            ]
        });
        let p = &mut params[param_no as usize];
        p.value = Some(value);
        p.type_ = type_;
        if type_ == MYSQL_TYPE_LONG_BLOB {
            p.blob_used = false;
        }
        s.send_types_to_server = true;
        Ok(())
    }

    /// `mysqlnd_stmt::refresh_bind_param`: forces types to be resent.
    pub fn refresh_bind_param(&self) -> Result<()> {
        let mut s = self.data();
        if s.state < StmtState::Prepared {
            s.set_error(CR_NO_PREPARE_STMT, MSG_STMT_NOT_PREPARED);
            drop(s);
            return self.fail();
        }
        s.error_info.reset();
        if s.param_count > 0 {
            s.send_types_to_server = true;
        }
        Ok(())
    }

    /// `mysqlnd_stmt::bind_result`: validates and records that result
    /// variables are bound (the values are returned by [`Statement::fetch`]).
    pub fn bind_result(&self) -> Result<()> {
        let mut s = self.data();
        if s.state < StmtState::Prepared {
            s.set_error(CR_NO_PREPARE_STMT, MSG_STMT_NOT_PREPARED);
            drop(s);
            return self.fail();
        }
        s.error_info.reset();
        if let Some(c) = &s.conn {
            c.data().error_info.reset();
        }
        if s.field_count > 0 {
            s.result_bound = true;
        }
        Ok(())
    }

    /// `mysqlnd_stmt::send_long_data` (`COM_STMT_SEND_LONG_DATA`, no reply).
    pub fn send_long_data(&self, param_no: u32, data: &[u8]) -> Result<()> {
        let mut s = self.data();
        s.error_info.reset();
        let Some(conn) = s.conn() else {
            return Err(Error {
                errno: 0,
                sqlstate: String::new(),
                message: Vec::new(),
            });
        };
        conn.data().error_info.reset();
        if s.state < StmtState::Prepared {
            s.set_error(CR_NO_PREPARE_STMT, MSG_STMT_NOT_PREPARED);
            drop(s);
            return self.fail();
        }
        if s.params.is_none() {
            s.set_error(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
            drop(s);
            return self.fail();
        }
        if param_no >= s.param_count {
            s.set_error(CR_INVALID_PARAMETER_NO, "Invalid parameter number");
            drop(s);
            return self.fail();
        }
        if s.params
            .as_ref()
            .is_some_and(|p| p[param_no as usize].type_ != MYSQL_TYPE_LONG_BLOB)
        {
            s.set_error(CR_INVALID_BUFFER_USE, MSG_NOT_BOUND_AS_BLOB);
            drop(s);
            return self.fail();
        }
        let mut ok = false;
        if conn.data().state == ConnState::Ready {
            if let Some(p) = &mut s.params {
                p[param_no as usize].blob_used = true;
            }
            let mut buf = Vec::with_capacity(6 + data.len());
            buf.extend_from_slice(&s.stmt_id.to_le_bytes());
            #[allow(clippy::cast_possible_truncation)]
            buf.extend_from_slice(&(param_no as u16).to_le_bytes());
            buf.extend_from_slice(data);
            ok = conn
                .data()
                .send_command(Command::StmtSendLongData, &buf, false);
            if !ok {
                copy_conn_error(&mut s);
            }
        }
        drop(s);
        if ok {
            Ok(())
        } else {
            self.fail()
        }
    }

    /// `mysqlnd_stmt::execute`. Parameter values are converted with PHP
    /// semantics through `ctx` (`__toString`, numeric strings, ...).
    pub fn execute(&self, ctx: &mut dyn RuntimeContext) -> std::result::Result<(), ExecuteError> {
        let mut s = self.data();
        let Some(conn) = s.conn() else {
            return Err(ExecuteError::Mysql(Error {
                errno: 0,
                sqlstate: String::new(),
                message: Vec::new(),
            }));
        };
        // send_execute
        s.upsert.set_affected_rows_to_error();
        conn.data().upsert.set_affected_rows_to_error();
        if (s.meta.is_some() || s.result.is_some())
            && s.state >= StmtState::Prepared
            && s.field_count > 0
        {
            s.flush();
            if let Some(r) = &mut s.result {
                r.release_buffers();
            }
            s.result = None;
            s.state = StmtState::Prepared;
        } else if s.state < StmtState::Prepared {
            s.set_error(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
            return Err(ExecuteError::Mysql(s.error_info.to_error()));
        }
        if s.param_count > 0 {
            let Some(params) = &s.params else {
                s.set_error(
                    CR_PARAMS_NOT_BOUND,
                    "No data supplied for parameters in prepared statement",
                );
                return Err(ExecuteError::Mysql(s.error_info.to_error()));
            };
            let not_bound = params.iter().filter(|p| p.value.is_none()).count();
            if not_bound > 0 {
                let msg = format!(
                    "No data supplied for {not_bound} parameter{} in prepared statement",
                    if not_bound > 1 { "s" } else { "" }
                );
                s.set_error(CR_PARAMS_NOT_BOUND, &msg);
                return Err(ExecuteError::Mysql(s.error_info.to_error()));
            }
        }
        let request = match s.generate_request(ctx) {
            Ok(r) => r,
            Err(e) => {
                s.set_error(
                    CR_UNKNOWN_ERROR,
                    "Couldn't generate the request. Possibly OOM.",
                );
                return Err(ExecuteError::Runtime(e));
            }
        };
        if !conn
            .data()
            .send_command(Command::StmtExecute, &request, false)
        {
            copy_conn_error(&mut s);
            return Err(ExecuteError::Mysql(s.error_info.to_error()));
        }
        s.execute_count += 1;
        if s.parse_execute_response() {
            Ok(())
        } else {
            Err(ExecuteError::Mysql(s.error_info.to_error()))
        }
    }

    /// `mysqlnd_stmt_store_result` (`PASS` when there is no result set).
    pub fn store_result(&self) -> Result<()> {
        let mut s = self.data();
        if s.field_count == 0 {
            return Ok(());
        }
        if s.store_result() {
            Ok(())
        } else {
            drop(s);
            self.fail()
        }
    }

    /// `mysqlnd_stmt::get_result`: a separate buffered result set.
    pub fn get_result(&self) -> Result<Option<ResultSet>> {
        let mut s = self.data();
        let Some(conn) = s.conn() else {
            return Ok(None);
        };
        let Some(meta) = s.meta.clone() else {
            return Ok(None);
        };
        if s.field_count == 0 {
            return Ok(None);
        }
        if !s.check_state(&conn) {
            s.set_error(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
            drop(s);
            return self.fail();
        }
        s.error_info.reset();
        conn.data().error_info.reset();
        conn.data().inc(Stat::BufferedSets, 1);
        if s.cursor_exists && !s.send_cursor_fetch(&conn, u32::MAX) {
            drop(s);
            return self.fail();
        }
        let rows = conn.data().fetch_all_rows(true);
        match rows {
            Ok(rows) => {
                s.upsert.affected_rows = rows.len() as u64;
                s.state = StmtState::Prepared;
                Ok(Some(ResultSet::buffered(Some(conn), meta, rows, true)))
            }
            Err(e) => {
                conn.data().error_info.set(e.errno, &e.sqlstate, &e.error);
                s.error_info.set(e.errno, &e.sqlstate, &e.error);
                s.state = StmtState::Prepared;
                drop(s);
                self.fail()
            }
        }
    }

    /// `mysqlnd_stmt::fetch`: the next row (`Ok(None)` = `MYSQL_NO_DATA`).
    pub fn fetch(&self) -> Result<Option<Vec<Value>>> {
        let mut s = self.data();
        let Some(conn) = s.conn() else {
            return Err(Error {
                errno: 0,
                sqlstate: String::new(),
                message: Vec::new(),
            });
        };
        if s.meta.is_none() || s.state < StmtState::WaitingUseOrStore {
            s.set_error(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
            drop(s);
            return self.fail();
        } else if s.state == StmtState::WaitingUseOrStore {
            s.run_handler();
        }
        s.state = StmtState::UserFetching;
        s.error_info.reset();
        conn.data().error_info.reset();
        if s.result
            .as_ref()
            .is_some_and(|r| r.cursor_stmt_id.is_some())
        {
            return Self::fetch_cursor(&mut s, &conn);
        }
        let Some(r) = s.result.as_mut() else {
            s.set_error(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
            drop(s);
            return self.fail();
        };
        match r.fetch_row_values() {
            Ok(v) => Ok(v),
            Err(e) => {
                if e.errno != 0 {
                    s.error_info.set(e.errno, &e.sqlstate, &e.message);
                }
                Err(e)
            }
        }
    }

    /// `mysqlnd_fetch_stmt_row_cursor`
    fn fetch_cursor(s: &mut StmtData, conn: &Connection) -> Result<Option<Vec<Value>>> {
        if s.result.as_ref().is_some_and(ResultSet::eof) {
            return Ok(None);
        }
        if !s.send_cursor_fetch(conn, 1) {
            return Err(s.error_info.to_error());
        }
        s.upsert.reset();
        let mut prev = 0;
        let pkt = conn.data().read_row(true, &mut prev);
        let (out, status) = match pkt {
            // Past `memory_limit`: no row, but the EOF that follows is read.
            RowPacket::OverLimit => {
                let eof = conn.data().read_row(true, &mut prev);
                let status = match eof {
                    RowPacket::Eof(e) => Some((e.warning_count, e.server_status)),
                    _ => None,
                };
                (Ok(None), status)
            }
            RowPacket::Row(raw) => {
                let r = s
                    .result
                    .as_mut()
                    .and_then(|r| r.cursor_row(Some(&raw), conn));
                // we asked for one row: the EOF follows
                let eof = conn.data().read_row(true, &mut prev);
                conn.data().inc(Stat::RowsFetchedFromClientPsCursor, 1);
                let status = match eof {
                    RowPacket::Eof(e) => Some((e.warning_count, e.server_status)),
                    _ => None,
                };
                match r {
                    Some(v) => (Ok(v), status),
                    None => (
                        Err(Error {
                            errno: 0,
                            sqlstate: String::new(),
                            message: Vec::new(),
                        }),
                        status,
                    ),
                }
            }
            RowPacket::Eof(e) => {
                if let Some(r) = s.result.as_mut() {
                    r.mark_eof();
                }
                (Ok(None), Some((e.warning_count, e.server_status)))
            }
            RowPacket::Err(e) => {
                e.apply(&mut conn.data().error_info);
                e.apply(&mut s.error_info);
                (Err(s.error_info.to_error()), Some((0, 0)))
            }
        };
        if let Some((w, st)) = status {
            s.upsert.warning_count = u32::from(w);
            s.upsert.server_status = st;
            let mut c = conn.data();
            c.upsert.warning_count = u32::from(w);
            c.upsert.server_status = st;
        }
        out
    }

    /// `mysqlnd_stmt::reset` (`COM_STMT_RESET`).
    pub fn reset(&self) -> Result<()> {
        let mut s = self.data();
        let Some(conn) = s.conn() else {
            return Err(Error {
                errno: 0,
                sqlstate: String::new(),
                message: Vec::new(),
            });
        };
        s.error_info.reset();
        conn.data().error_info.reset();
        let mut ok = true;
        if s.stmt_id != 0 {
            if let Some(ps) = &mut s.params {
                for p in ps.iter_mut() {
                    p.blob_used = false;
                }
            }
            s.flush();
            if conn.data().state == ConnState::Ready {
                let id = s.stmt_id;
                let mut c = conn.data();
                ok = c.send_command(Command::StmtReset, &id.to_le_bytes(), false)
                    && c.handle_response_pub(false, false, Command::StmtReset, true);
                drop(c);
                if !ok {
                    copy_conn_error(&mut s);
                }
            }
            s.upsert = conn.data().upsert;
        }
        drop(s);
        if ok {
            Ok(())
        } else {
            self.fail()
        }
    }

    /// `mysqlnd_stmt::free_result`: drops the client-side buffers.
    pub fn free_result(&self) {
        let mut s = self.data();
        if s.meta.is_none() && s.result.is_none() {
            return;
        }
        if s.state == StmtState::WaitingUseOrStore {
            s.handler = RsetHandler::Use;
            s.run_handler();
        }
        if s.state > StmtState::WaitingUseOrStore {
            if let Some(r) = &mut s.result {
                r.release_buffers();
            }
            s.result = None;
        }
        if s.state > StmtState::Prepared {
            s.state = StmtState::Prepared;
        }
    }

    /// `mysqlnd_stmt_close` (explicit) — also done implicitly on drop.
    pub fn close(&self) -> bool {
        let mut s = self.data();
        if s.closed {
            return true;
        }
        s.dtor(false)
    }

    /// `mysqlnd_stmt::more_results`
    #[must_use]
    pub fn more_results(&self) -> bool {
        self.data().more_results()
    }

    /// `mysqlnd_stmt::next_result`
    pub fn next_result(&self) -> Result<bool> {
        let mut s = self.data();
        let pending = s.conn.as_ref().is_some_and(|c| {
            let d = c.data();
            d.state == ConnState::NextResultPending
                && d.upsert.server_status & SERVER_MORE_RESULTS_EXISTS != 0
        });
        if !pending || (s.meta.is_none() && s.result.is_none()) {
            return Ok(false);
        }
        if s.next_result() {
            Ok(true)
        } else {
            drop(s);
            self.fail()
        }
    }

    /// `mysqlnd_stmt::result_metadata`: a field-only result set.
    #[must_use]
    pub fn result_metadata(&self) -> Option<ResultSet> {
        let s = self.data();
        if s.field_count == 0 {
            return None;
        }
        let meta = s.meta.clone()?;
        Some(ResultSet::metadata_only(s.conn.clone(), meta))
    }

    /// `mysqlnd_stmt::data_seek` (buffered sets).
    pub fn data_seek(&self, row: u64) -> bool {
        self.data()
            .result
            .as_mut()
            .is_some_and(|r| r.data_seek(row))
    }

    /// `mysqlnd_stmt::attr_set`
    pub fn attr_set(&self, attr: u32, value: u64) -> Result<()> {
        let mut s = self.data();
        match attr {
            STMT_ATTR_UPDATE_MAX_LENGTH => s.update_max_length = value != 0,
            STMT_ATTR_CURSOR_TYPE => {
                if value > CURSOR_TYPE_READ_ONLY {
                    s.set_error(CR_NOT_IMPLEMENTED, "Not implemented");
                    drop(s);
                    return self.fail();
                }
                s.flags = value;
            }
            _ => {
                s.set_error(CR_NOT_IMPLEMENTED, "Not implemented");
                drop(s);
                return self.fail();
            }
        }
        Ok(())
    }

    /// `mysqlnd_stmt::attr_get`
    #[must_use]
    pub fn attr_get(&self, attr: u32) -> Option<u64> {
        let s = self.data();
        match attr {
            STMT_ATTR_UPDATE_MAX_LENGTH => Some(u64::from(s.update_max_length)),
            STMT_ATTR_CURSOR_TYPE => Some(s.flags),
            _ => None,
        }
    }

    /// `mysqlnd_stmt_param_count`
    #[must_use]
    pub fn param_count(&self) -> u32 {
        self.data().param_count
    }

    /// `mysqlnd_stmt_field_count`
    #[must_use]
    pub fn field_count(&self) -> u32 {
        self.data().field_count
    }

    /// `mysqlnd_stmt_num_rows`
    #[must_use]
    pub fn num_rows(&self) -> u64 {
        self.data().result.as_ref().map_or(0, ResultSet::num_rows)
    }

    /// `mysqlnd_stmt_affected_rows` (`u64::MAX` = -1)
    #[must_use]
    pub fn affected_rows(&self) -> u64 {
        self.data().upsert.affected_rows
    }

    /// `mysqlnd_stmt_insert_id`
    #[must_use]
    pub fn insert_id(&self) -> u64 {
        self.data().upsert.last_insert_id
    }

    /// `mysqlnd_stmt_warning_count`
    #[must_use]
    pub fn warning_count(&self) -> u32 {
        self.data().upsert.warning_count
    }

    /// `mysqlnd_stmt_server_status`
    #[must_use]
    pub fn server_status(&self) -> u16 {
        self.data().upsert.server_status
    }

    /// `mysqlnd_stmt_errno`
    #[must_use]
    pub fn errno(&self) -> u32 {
        self.data().error_info.errno
    }

    /// `mysqlnd_stmt_error`
    #[must_use]
    pub fn error(&self) -> Vec<u8> {
        self.data().error_info.error.clone()
    }

    /// `mysqlnd_stmt_sqlstate`
    #[must_use]
    pub fn sqlstate(&self) -> String {
        self.data().error_info.sqlstate_or_null().to_string()
    }

    /// `mysqli_stmt_error_list`
    #[must_use]
    pub fn error_list(&self) -> Vec<crate::error::ErrorEntry> {
        self.data().error_info.list.clone()
    }

    /// Statement id assigned by the server (0 before prepare).
    #[must_use]
    pub fn id(&self) -> u32 {
        self.data().stmt_id
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> StmtState {
        self.data().state
    }

    /// The result metadata from prepare / the last execute.
    #[must_use]
    pub fn metadata(&self) -> Option<Rc<Metadata>> {
        self.data().meta.clone()
    }

    /// The currently bound parameters.
    #[must_use]
    pub fn params(&self) -> Option<Vec<ParamBind>> {
        self.data().params.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::STMT_ATTR_PREFETCH_ROWS;

    #[test]
    fn type_chars() {
        assert_eq!(param_type_from_char(b'i'), Some(MYSQL_TYPE_LONGLONG));
        assert_eq!(param_type_from_char(b'd'), Some(MYSQL_TYPE_DOUBLE));
        assert_eq!(param_type_from_char(b's'), Some(MYSQL_TYPE_VAR_STRING));
        assert_eq!(param_type_from_char(b'b'), Some(MYSQL_TYPE_LONG_BLOB));
        assert_eq!(param_type_from_char(b'x'), None);
    }

    #[test]
    fn unprepared_statement_errors() {
        let c = Connection::init();
        let st = c.stmt_init();
        let e = st.bind_params(vec![]).unwrap_err();
        assert_eq!(e.errno, CR_NO_PREPARE_STMT);
        assert_eq!(e.message, b"Statement not prepared");
        assert_eq!(st.sqlstate(), "HY000");
        let e = st.attr_set(STMT_ATTR_PREFETCH_ROWS, 1).unwrap_err();
        assert_eq!(e.errno, CR_NOT_IMPLEMENTED);
        assert!(st.attr_set(STMT_ATTR_CURSOR_TYPE, 1).is_ok());
        assert_eq!(st.attr_get(STMT_ATTR_CURSOR_TYPE), Some(1));
    }
}
