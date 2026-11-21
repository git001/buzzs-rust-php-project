// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! What the SAPI needs from the web server for one request.
//!
//! [`Backend`] abstracts the `request_rec` operations used by
//! `sapi_apache2.c` / `php_functions.c`.  The real implementation
//! ([`crate::ffi::ApacheRequest`]) forwards to httpd through the C shim's
//! function table; [`MockBackend`] records everything in memory for unit
//! tests.

use std::cell::RefCell;

/// String fields of a `request_rec` (`enum phprs_str_field`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum StrField {
    /// `r->handler`.
    Handler = 0,
    /// `r->filename` (the script path).
    Filename = 1,
    /// `r->uri`.
    Uri = 2,
    /// `r->args` (query string).
    Args = 3,
    /// `r->method`.
    Method = 4,
    /// `r->protocol`.
    Protocol = 5,
    /// `r->user`.
    User = 6,
    /// `r->path_info`.
    PathInfo = 7,
    /// `r->the_request`.
    TheRequest = 8,
    /// `r->status_line`.
    StatusLine = 9,
    /// `r->content_type`.
    ContentType = 10,
    /// `r->unparsed_uri`.
    UnparsedUri = 11,
    /// `r->range`.
    Range = 12,
}

/// Integer fields of a `request_rec` (`enum phprs_int_field`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum IntField {
    /// `r->proto_num` (1001 = HTTP/1.1).
    ProtoNum = 0,
    /// `r->request_time` in microseconds.
    RequestTime = 1,
    /// `r->finfo.filetype`: 0 none, 1 regular file, 2 directory, 3 other.
    FileType = 2,
    /// Whether the file is user-executable (`XBitHack`).
    UserExec = 3,
    /// `AcceptPathInfo Off` and a non-empty `PATH_INFO`.
    PathInfoRejected = 4,
    /// Whether this is the main request (`r->main == NULL`).
    IsMain = 5,
    /// `r->status`.
    Status = 6,
    /// `r->mtime` in microseconds.
    Mtime = 7,
    /// `r->clength`.
    Clength = 8,
    /// `r->chunked`.
    Chunked = 9,
    /// `r->no_cache`.
    NoCache = 10,
    /// `r->no_local_copy`.
    NoLocalCopy = 11,
    /// `r->allowed`.
    Allowed = 12,
    /// `r->sent_bodyct`.
    SentBodyct = 13,
    /// `r->bytes_sent`.
    BytesSent = 14,
    /// `r->connection->aborted`.
    Aborted = 15,
    /// A sub-request whose environment differs from the main request's.
    EnvDiffers = 16,
    /// `r->header_only` (HEAD request).
    HeaderOnly = 17,
}

/// Tables of a `request_rec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Table {
    /// `r->headers_in`.
    HeadersIn = 0,
    /// `r->headers_out`.
    HeadersOut = 1,
    /// `r->subprocess_env`.
    SubprocessEnv = 2,
    /// `r->notes`.
    Notes = 3,
    /// `r->err_headers_out`.
    ErrHeadersOut = 4,
}

/// `apr_table_*` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum TableOp {
    /// `apr_table_set`.
    Set = 0,
    /// `apr_table_add`.
    Add = 1,
    /// `apr_table_unset`.
    Unset = 2,
    /// `apr_table_clear`.
    Clear = 3,
}

/// httpd log levels (`APLOG_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub enum LogLevel {
    /// `APLOG_EMERG`.
    Emerg = 0,
    /// `APLOG_ALERT`.
    Alert = 1,
    /// `APLOG_CRIT`.
    Crit = 2,
    /// `APLOG_ERR`.
    Err = 3,
    /// `APLOG_WARNING`.
    Warning = 4,
    /// `APLOG_NOTICE`.
    Notice = 5,
    /// `APLOG_INFO`.
    Info = 6,
    /// `APLOG_DEBUG`.
    Debug = 7,
}

impl LogLevel {
    /// The level `php_apache_sapi_log_message()` logs a syslog priority
    /// at (`APLOG_*` equal `LOG_*`); anything else, such as the -1 of
    /// `error_log($message, 4)`, is `APLOG_ERR`.
    #[must_use]
    pub fn from_syslog(severity: i32) -> Self {
        match severity {
            0 => LogLevel::Emerg,
            1 => LogLevel::Alert,
            2 => LogLevel::Crit,
            4 => LogLevel::Warning,
            5 => LogLevel::Notice,
            6 => LogLevel::Info,
            7 => LogLevel::Debug,
            _ => LogLevel::Err,
        }
    }
}

/// The INI access level of a per-directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirStatus {
    /// `php_value` / `php_flag` (`PHP_INI_PERDIR`).
    PerDir,
    /// `php_admin_value` / `php_admin_flag` (`PHP_INI_SYSTEM`).
    System,
}

/// One `php_value`-family directive in effect for the request (after
/// Apache merged the per-directory configurations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// INI directive name.
    pub name: String,
    /// Value (`none` already mapped to empty, flags to `0`/`1`).
    pub value: Vec<u8>,
    /// Access level.
    pub status: DirStatus,
    /// Whether it came from a `.htaccess` file.
    pub htaccess: bool,
}

/// A property value of `apache_lookup_uri()`'s result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupValue {
    /// `add_property_long`.
    Int(i64),
    /// `add_property_string`.
    Str(Vec<u8>),
}

/// The fields of a sub-request exposed by `apache_lookup_uri()`, in
/// `php_functions.c` order (a repeated name overwrites the earlier value
/// in place, as `add_property_*` does).
pub type LookupInfo = Vec<(&'static str, LookupValue)>;

/// A looked-up sub-request (`ap_sub_req_lookup_uri`); destroyed on drop.
pub trait SubRequest {
    /// `rr->status`.
    fn status(&self) -> i32;
    /// The `apache_lookup_uri()` property list (in C order).
    fn info(&self) -> LookupInfo;
    /// `ap_run_sub_req()`; returns its result (0 = OK).
    fn run(&self) -> i32;
}

/// The web-server side of one request.
pub trait Backend {
    /// A string field.
    fn str_field(&self, field: StrField) -> Option<Vec<u8>>;
    /// An integer field.
    fn int_field(&self, field: IntField) -> i64;
    /// Sets `r->user`.
    fn set_user(&self, user: Option<&[u8]>);
    /// `ap_add_common_vars()` + `ap_add_cgi_vars()`.
    fn add_cgi_vars(&self);
    /// All entries of a table, in order.
    fn table_entries(&self, table: Table) -> Vec<(Vec<u8>, Vec<u8>)>;
    /// `apr_table_get` (case-insensitive, first match).
    fn table_get(&self, table: Table, key: &[u8]) -> Option<Vec<u8>>;
    /// `apr_table_set/add/unset/clear`.
    fn table_op(&self, table: Table, op: TableOp, key: &[u8], value: &[u8]);
    /// Sets `r->status`, optionally `r->status_line` and `r->proto_num`
    /// (`proto_num <= 0` leaves it alone).
    fn set_status(&self, status: i32, status_line: Option<&[u8]>, proto_num: i32);
    /// `ap_set_content_type`.
    fn set_content_type(&self, content_type: &[u8]);
    /// `ap_set_content_length`.
    fn set_content_length(&self, len: i64);
    /// `ap_rwrite`; `false` when the client is gone.
    fn write(&self, data: &[u8]) -> bool;
    /// `ap_rflush`; `false` when the client is gone.
    fn flush(&self) -> bool;
    /// `ap_setup_client_block` + `ap_should_client_block`: `Ok(true)` when a
    /// body follows, `Err(status)` on a protocol error.
    fn body_setup(&self) -> Result<bool, i32>;
    /// `ap_get_client_block`: bytes read, 0 at the end, negative on error.
    fn body_read(&self, buf: &mut [u8]) -> isize;
    /// `ap_log_rerror(APLOG_MARK, level, 0, r, "%s", msg)`.
    fn log(&self, level: LogLevel, msg: &[u8]);
    /// The merged `php_value`-family directives for this request.
    fn dir_config(&self) -> Vec<DirEntry>;
    /// `ap_update_mtime` + `ap_set_last_modified`.
    fn set_last_modified(&self) {}
    /// `ap_get_server_banner()`.
    fn server_banner(&self) -> Vec<u8> {
        Vec::new()
    }
    /// Names of the loaded modules (`ap_loaded_modules[i]->name`).
    fn loaded_modules(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// `ap_sub_req_lookup_uri(uri, r, r->output_filters)`.
    fn lookup_uri(&self, _uri: &[u8]) -> Option<Box<dyn SubRequest>> {
        None
    }
    /// The first request of an internal redirect chain (`r->prev`*), if
    /// different from this one.
    fn top_request(&self) -> Option<Box<dyn Backend>> {
        None
    }
}

/// Shared backends (tests keep a handle to inspect what a request did).
impl<T: Backend + ?Sized> Backend for std::rc::Rc<T> {
    fn str_field(&self, field: StrField) -> Option<Vec<u8>> {
        (**self).str_field(field)
    }
    fn int_field(&self, field: IntField) -> i64 {
        (**self).int_field(field)
    }
    fn set_user(&self, user: Option<&[u8]>) {
        (**self).set_user(user);
    }
    fn add_cgi_vars(&self) {
        (**self).add_cgi_vars();
    }
    fn table_entries(&self, table: Table) -> Vec<(Vec<u8>, Vec<u8>)> {
        (**self).table_entries(table)
    }
    fn table_get(&self, table: Table, key: &[u8]) -> Option<Vec<u8>> {
        (**self).table_get(table, key)
    }
    fn table_op(&self, table: Table, op: TableOp, key: &[u8], value: &[u8]) {
        (**self).table_op(table, op, key, value);
    }
    fn set_status(&self, status: i32, status_line: Option<&[u8]>, proto_num: i32) {
        (**self).set_status(status, status_line, proto_num);
    }
    fn set_content_type(&self, content_type: &[u8]) {
        (**self).set_content_type(content_type);
    }
    fn set_content_length(&self, len: i64) {
        (**self).set_content_length(len);
    }
    fn write(&self, data: &[u8]) -> bool {
        (**self).write(data)
    }
    fn flush(&self) -> bool {
        (**self).flush()
    }
    fn body_setup(&self) -> Result<bool, i32> {
        (**self).body_setup()
    }
    fn body_read(&self, buf: &mut [u8]) -> isize {
        (**self).body_read(buf)
    }
    fn log(&self, level: LogLevel, msg: &[u8]) {
        (**self).log(level, msg);
    }
    fn dir_config(&self) -> Vec<DirEntry> {
        (**self).dir_config()
    }
    fn set_last_modified(&self) {
        (**self).set_last_modified();
    }
    fn server_banner(&self) -> Vec<u8> {
        (**self).server_banner()
    }
    fn loaded_modules(&self) -> Vec<Vec<u8>> {
        (**self).loaded_modules()
    }
    fn lookup_uri(&self, uri: &[u8]) -> Option<Box<dyn SubRequest>> {
        (**self).lookup_uri(uri)
    }
    fn top_request(&self) -> Option<Box<dyn Backend>> {
        (**self).top_request()
    }
}

/// The entries of a table.
pub type Entries = Vec<(Vec<u8>, Vec<u8>)>;

/// The arguments of a `set_status` call: status, status line, protocol.
pub type StatusCall = (i32, Option<Vec<u8>>, i32);

/// An in-memory [`Backend`] for tests: tables are ordered lists with
/// case-insensitive keys, output and log lines are recorded.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct MockBackend {
    /// String fields.
    pub strs: RefCell<Vec<(StrField, Vec<u8>)>>,
    /// Integer fields.
    pub ints: RefCell<Vec<(IntField, i64)>>,
    /// Tables.
    pub tables: RefCell<Vec<(Table, Entries)>>,
    /// `(status, status_line, proto_num)` of the last `set_status`.
    pub status: RefCell<Option<StatusCall>>,
    /// Last content type.
    pub content_type: RefCell<Option<Vec<u8>>>,
    /// Last content length.
    pub content_length: RefCell<Option<i64>>,
    /// Response body.
    pub body_out: RefCell<Vec<u8>>,
    /// Number of flushes.
    pub flushes: RefCell<usize>,
    /// Request body still to be read.
    pub body_in: RefCell<Vec<u8>>,
    /// Log lines.
    pub logs: RefCell<Vec<(LogLevel, Vec<u8>)>>,
    /// Per-directory configuration.
    pub dir: RefCell<Vec<DirEntry>>,
    /// Simulate a client that went away.
    pub disconnected: RefCell<bool>,
}

impl MockBackend {
    /// Sets a string field.
    #[must_use]
    pub fn with_str(self, f: StrField, v: &[u8]) -> Self {
        self.strs.borrow_mut().push((f, v.to_vec()));
        self
    }

    /// Sets an integer field.
    #[must_use]
    pub fn with_int(self, f: IntField, v: i64) -> Self {
        self.ints.borrow_mut().push((f, v));
        self
    }

    /// Adds a table entry.
    #[must_use]
    pub fn with_entry(self, t: Table, k: &[u8], v: &[u8]) -> Self {
        self.table_op(t, TableOp::Add, k, v);
        self
    }

    fn with_table<R>(&self, t: Table, f: impl FnOnce(&mut Vec<(Vec<u8>, Vec<u8>)>) -> R) -> R {
        let mut tables = self.tables.borrow_mut();
        let idx = if let Some(i) = tables.iter().position(|(x, _)| *x == t) {
            i
        } else {
            tables.push((t, Vec::new()));
            tables.len() - 1
        };
        f(&mut tables[idx].1)
    }
}

impl Backend for MockBackend {
    fn str_field(&self, field: StrField) -> Option<Vec<u8>> {
        self.strs
            .borrow()
            .iter()
            .rev()
            .find(|(f, _)| *f == field)
            .map(|(_, v)| v.clone())
    }

    fn int_field(&self, field: IntField) -> i64 {
        self.ints
            .borrow()
            .iter()
            .rev()
            .find(|(f, _)| *f == field)
            .map_or(0, |(_, v)| *v)
    }

    fn set_user(&self, user: Option<&[u8]>) {
        let mut s = self.strs.borrow_mut();
        s.retain(|(f, _)| *f != StrField::User);
        if let Some(u) = user {
            s.push((StrField::User, u.to_vec()));
        }
    }

    fn add_cgi_vars(&self) {}

    fn table_entries(&self, table: Table) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.with_table(table, |t| t.clone())
    }

    fn table_get(&self, table: Table, key: &[u8]) -> Option<Vec<u8>> {
        self.with_table(table, |t| {
            t.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.clone())
        })
    }

    fn table_op(&self, table: Table, op: TableOp, key: &[u8], value: &[u8]) {
        self.with_table(table, |t| match op {
            TableOp::Set => {
                t.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
                t.push((key.to_vec(), value.to_vec()));
            }
            TableOp::Add => t.push((key.to_vec(), value.to_vec())),
            TableOp::Unset => t.retain(|(k, _)| !k.eq_ignore_ascii_case(key)),
            TableOp::Clear => t.clear(),
        });
    }

    fn set_status(&self, status: i32, status_line: Option<&[u8]>, proto_num: i32) {
        *self.status.borrow_mut() = Some((status, status_line.map(<[u8]>::to_vec), proto_num));
    }

    fn set_content_type(&self, content_type: &[u8]) {
        *self.content_type.borrow_mut() = Some(content_type.to_vec());
    }

    fn set_content_length(&self, len: i64) {
        *self.content_length.borrow_mut() = Some(len);
    }

    fn write(&self, data: &[u8]) -> bool {
        if *self.disconnected.borrow() {
            return false;
        }
        self.body_out.borrow_mut().extend_from_slice(data);
        true
    }

    fn flush(&self) -> bool {
        *self.flushes.borrow_mut() += 1;
        !*self.disconnected.borrow()
    }

    fn body_setup(&self) -> Result<bool, i32> {
        Ok(!self.body_in.borrow().is_empty())
    }

    fn body_read(&self, buf: &mut [u8]) -> isize {
        let mut body = self.body_in.borrow_mut();
        let n = buf.len().min(body.len());
        buf[..n].copy_from_slice(&body[..n]);
        body.drain(..n);
        isize::try_from(n).unwrap_or(0)
    }

    fn log(&self, level: LogLevel, msg: &[u8]) {
        self.logs.borrow_mut().push((level, msg.to_vec()));
    }

    fn dir_config(&self) -> Vec<DirEntry> {
        self.dir.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_tables_are_case_insensitive() {
        let m = MockBackend::default();
        m.table_op(Table::HeadersOut, TableOp::Add, b"X-A", b"1");
        m.table_op(Table::HeadersOut, TableOp::Add, b"x-a", b"2");
        assert_eq!(m.table_get(Table::HeadersOut, b"X-a"), Some(b"1".to_vec()));
        m.table_op(Table::HeadersOut, TableOp::Set, b"X-A", b"3");
        assert_eq!(
            m.table_entries(Table::HeadersOut),
            vec![(b"X-A".to_vec(), b"3".to_vec())]
        );
        m.table_op(Table::HeadersOut, TableOp::Unset, b"x-A", b"");
        assert!(m.table_entries(Table::HeadersOut).is_empty());
    }

    #[test]
    fn mock_body_reads_in_chunks() {
        let m = MockBackend::default();
        m.body_in.borrow_mut().extend_from_slice(b"hello world");
        let mut buf = [0u8; 4];
        assert_eq!(m.body_read(&mut buf), 4);
        assert_eq!(&buf, b"hell");
        let mut rest = [0u8; 64];
        assert_eq!(m.body_read(&mut rest), 7);
        assert_eq!(m.body_read(&mut rest), 0);
    }
}
