// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The FFI boundary with the C shim (`csrc/mod_phprust.c`).
//!
//! * The shim hands Rust a table of `request_rec` accessors
//!   ([`ApiTable`], mirrored from `csrc/phprs_api.h`) when httpd registers
//!   the module's hooks; [`ApacheRequest`] wraps a `request_rec *` and
//!   implements [`Backend`] through it.  Rust never declares httpd struct
//!   layouts and never links against httpd symbols.
//! * The `extern "C"` entry points (`phprust_*`) are what the shim's hooks
//!   call.
//! * `php_module` is opaque, writable storage for httpd's `module` struct:
//!   rustc only exports Rust symbols from a cdylib, so the shim's load-time
//!   constructor copies its module definition here.
//!
//! All `unsafe` code of the crate lives here (plus `libc` calls for
//! `mkstemp`/`umask`/fd redirection documented at their call sites).

use crate::backend::{
    Backend, DirEntry, DirStatus, IntField, LogLevel, LookupInfo, LookupValue, StrField,
    SubRequest, Table, TableOp,
};
use std::cell::UnsafeCell;
use std::ffi::{c_char, c_int, c_long, c_longlong, c_void, CStr, CString};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;

/// `phprs_api` (keep in sync with `csrc/phprs_api.h`).
#[repr(C)]
#[derive(Debug)]
#[allow(clippy::struct_field_names)]
pub struct ApiTable {
    size: usize,
    r_str: unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char,
    r_int: unsafe extern "C" fn(*mut c_void, c_int) -> c_longlong,
    set_user: unsafe extern "C" fn(*mut c_void, *const c_char),
    add_cgi_vars: unsafe extern "C" fn(*mut c_void),
    table_count: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    table_entry:
        unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut *const c_char, *mut *const c_char),
    table_get: unsafe extern "C" fn(*mut c_void, c_int, *const c_char) -> *const c_char,
    table_op: unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char, *const c_char),
    set_status: unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int),
    set_content_type: unsafe extern "C" fn(*mut c_void, *const c_char),
    set_content_length: unsafe extern "C" fn(*mut c_void, c_longlong),
    write: unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> c_int,
    flush: unsafe extern "C" fn(*mut c_void) -> c_int,
    body_setup: unsafe extern "C" fn(*mut c_void) -> c_int,
    body_read: unsafe extern "C" fn(*mut c_void, *mut c_char, usize) -> c_long,
    log_request: unsafe extern "C" fn(*mut c_void, c_int, *const c_char),
    log_server: unsafe extern "C" fn(c_int, *const c_char),
    server_banner: unsafe extern "C" fn() -> *const c_char,
    module_count: unsafe extern "C" fn() -> c_int,
    module_name: unsafe extern "C" fn(c_int) -> *const c_char,
    dir_count: unsafe extern "C" fn(*mut c_void) -> c_int,
    dir_entry: unsafe extern "C" fn(
        *mut c_void,
        c_int,
        *mut *const c_char,
        *mut *const c_char,
        *mut c_int,
        *mut c_int,
    ),
    sub_lookup: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    sub_run: unsafe extern "C" fn(*mut c_void) -> c_int,
    sub_destroy: unsafe extern "C" fn(*mut c_void),
    set_last_modified: unsafe extern "C" fn(*mut c_void),
    top_request: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    main_request: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
}

static API: AtomicPtr<ApiTable> = AtomicPtr::new(std::ptr::null_mut());

/// The registered table, if the shim installed one.
fn api() -> Option<&'static ApiTable> {
    let p = API.load(Ordering::Acquire);
    // SAFETY: the pointer is either null or the shim's static, immutable
    // `phprs_api_table`, which lives as long as the loaded module.
    unsafe { p.as_ref() }
}

/// Storage for httpd's `module` struct, filled by the shim's constructor.
#[repr(C, align(16))]
#[derive(Debug)]
pub struct ModuleStorage(UnsafeCell<[u8; 512]>);

// SAFETY: httpd accesses the module struct from its own (configuration)
// threads; Rust never reads or writes it.
unsafe impl Sync for ModuleStorage {}

/// `module AP_MODULE_DECLARE_DATA php_module` (see the module docs).
#[no_mangle]
#[used]
#[allow(non_upper_case_globals)]
pub static php_module: ModuleStorage = ModuleStorage(UnsafeCell::new([0; 512]));

/// Converts a nullable C string.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated string valid for the call.
unsafe fn bytes_of(p: *const c_char) -> Option<Vec<u8>> {
    if p.is_null() {
        return None;
    }
    // SAFETY: guaranteed by the caller.
    Some(unsafe { CStr::from_ptr(p) }.to_bytes().to_vec())
}

/// A C string for `s`, cut at the first NUL (C semantics).
fn cstring(s: &[u8]) -> CString {
    let s = s.split(|&c| c == 0).next().unwrap_or_default();
    CString::new(s).unwrap_or_default()
}

/// A `request_rec *` seen through the shim's accessor table.
///
/// It is created on the httpd thread that runs the handler and may be
/// moved to the PHP request thread while the httpd thread blocks waiting
/// for it (see `handler.rs`), so the request is never used concurrently.
#[derive(Debug)]
pub struct ApacheRequest {
    r: *mut c_void,
    api: &'static ApiTable,
}

/// A `request_rec *` that may cross to the PHP request thread.
#[derive(Debug, Clone, Copy)]
pub struct SendRequest(*mut c_void);

// SAFETY: the pointer is only dereferenced (through the accessor table) by
// one thread at a time: the httpd worker thread blocks until the request
// thread that received it has finished.
unsafe impl Send for SendRequest {}

impl SendRequest {
    /// Re-creates the request handle on the receiving thread.
    pub fn into_request(self) -> Option<ApacheRequest> {
        Some(ApacheRequest {
            r: self.0,
            api: api()?,
        })
    }
}

impl ApacheRequest {
    /// Wraps `r`.
    ///
    /// # Safety
    /// `r` must be a live `request_rec *` for as long as the handle (and
    /// any [`SendRequest`] made from it) is used.
    pub unsafe fn new(r: *mut c_void) -> Option<Self> {
        if r.is_null() {
            return None;
        }
        Some(ApacheRequest { r, api: api()? })
    }

    /// A sendable copy of the pointer.
    pub fn sendable(&self) -> SendRequest {
        SendRequest(self.r)
    }

    fn sstr(&self, f: c_int) -> Option<Vec<u8>> {
        // SAFETY: `r` is live (constructor contract); r_str returns a
        // pool-owned NUL-terminated string or null.
        unsafe { bytes_of((self.api.r_str)(self.r, f)) }
    }

    fn sint(&self, f: c_int) -> i64 {
        // SAFETY: `r` is live (constructor contract).
        unsafe { (self.api.r_int)(self.r, f) }
    }
}

impl Backend for ApacheRequest {
    fn str_field(&self, field: StrField) -> Option<Vec<u8>> {
        self.sstr(field as c_int)
    }

    fn int_field(&self, field: IntField) -> i64 {
        self.sint(field as c_int)
    }

    fn set_user(&self, user: Option<&[u8]>) {
        let c = user.map(cstring);
        // SAFETY: `r` is live; the shim copies the string into the pool.
        unsafe { (self.api.set_user)(self.r, c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr())) }
    }

    fn add_cgi_vars(&self) {
        // SAFETY: `r` is live.
        unsafe { (self.api.add_cgi_vars)(self.r) }
    }

    fn table_entries(&self, table: Table) -> Vec<(Vec<u8>, Vec<u8>)> {
        // SAFETY: `r` is live; entries are pool-owned C strings (or null).
        unsafe {
            let n = (self.api.table_count)(self.r, table as c_int);
            let mut out = Vec::with_capacity(usize::try_from(n).unwrap_or(0));
            for i in 0..n {
                let mut k: *const c_char = std::ptr::null();
                let mut v: *const c_char = std::ptr::null();
                (self.api.table_entry)(self.r, table as c_int, i, &raw mut k, &raw mut v);
                if let Some(k) = bytes_of(k) {
                    out.push((k, bytes_of(v).unwrap_or_default()));
                }
            }
            out
        }
    }

    fn table_get(&self, table: Table, key: &[u8]) -> Option<Vec<u8>> {
        let k = cstring(key);
        // SAFETY: `r` is live; `k` outlives the call.
        unsafe { bytes_of((self.api.table_get)(self.r, table as c_int, k.as_ptr())) }
    }

    fn table_op(&self, table: Table, op: TableOp, key: &[u8], value: &[u8]) {
        let k = cstring(key);
        let v = cstring(value);
        // SAFETY: `r` is live; APR copies keys and values.
        unsafe { (self.api.table_op)(self.r, table as c_int, op as c_int, k.as_ptr(), v.as_ptr()) }
    }

    fn set_status(&self, status: i32, status_line: Option<&[u8]>, proto_num: i32) {
        let line = status_line.map(cstring);
        // SAFETY: `r` is live; the shim copies the status line.
        unsafe {
            (self.api.set_status)(
                self.r,
                status,
                line.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                proto_num,
            );
        }
    }

    fn set_content_type(&self, content_type: &[u8]) {
        let c = cstring(content_type);
        // SAFETY: `r` is live; the shim copies the string.
        unsafe { (self.api.set_content_type)(self.r, c.as_ptr()) }
    }

    fn set_content_length(&self, len: i64) {
        // SAFETY: `r` is live.
        unsafe { (self.api.set_content_length)(self.r, len) }
    }

    fn write(&self, data: &[u8]) -> bool {
        // SAFETY: `r` is live; `data` is valid for `len` bytes.
        unsafe { (self.api.write)(self.r, data.as_ptr().cast(), data.len()) == 0 }
    }

    fn flush(&self) -> bool {
        // SAFETY: `r` is live.
        unsafe { (self.api.flush)(self.r) == 0 }
    }

    fn body_setup(&self) -> Result<bool, i32> {
        // SAFETY: `r` is live.
        let rc = unsafe { (self.api.body_setup)(self.r) };
        if rc < 0 {
            Err(-rc)
        } else {
            Ok(rc > 0)
        }
    }

    fn body_read(&self, buf: &mut [u8]) -> isize {
        // SAFETY: `r` is live; `buf` is writable for its length.
        let n = unsafe { (self.api.body_read)(self.r, buf.as_mut_ptr().cast(), buf.len()) };
        isize::try_from(n).unwrap_or(-1)
    }

    fn log(&self, level: LogLevel, msg: &[u8]) {
        let c = cstring(msg);
        // SAFETY: `r` is live; the message is logged with a "%s" format.
        unsafe { (self.api.log_request)(self.r, level as c_int, c.as_ptr()) }
    }

    fn dir_config(&self) -> Vec<DirEntry> {
        // SAFETY: `r` is live; entries are config-pool strings.
        unsafe {
            let n = (self.api.dir_count)(self.r);
            let mut out = Vec::new();
            for i in 0..n {
                let mut name: *const c_char = std::ptr::null();
                let mut value: *const c_char = std::ptr::null();
                let mut status: c_int = 0;
                let mut htaccess: c_int = 0;
                (self.api.dir_entry)(
                    self.r,
                    i,
                    &raw mut name,
                    &raw mut value,
                    &raw mut status,
                    &raw mut htaccess,
                );
                let Some(name) = bytes_of(name) else {
                    continue;
                };
                out.push(DirEntry {
                    name: String::from_utf8_lossy(&name).into_owned(),
                    value: bytes_of(value).unwrap_or_default(),
                    status: if status == 4 {
                        DirStatus::System
                    } else {
                        DirStatus::PerDir
                    },
                    htaccess: htaccess != 0,
                });
            }
            out
        }
    }

    fn set_last_modified(&self) {
        // SAFETY: `r` is live.
        unsafe { (self.api.set_last_modified)(self.r) }
    }

    fn server_banner(&self) -> Vec<u8> {
        // SAFETY: returns a static banner string.
        unsafe { bytes_of((self.api.server_banner)()) }.unwrap_or_default()
    }

    fn loaded_modules(&self) -> Vec<Vec<u8>> {
        // SAFETY: indices are within ap_loaded_modules' NULL terminator.
        unsafe {
            let n = (self.api.module_count)();
            (0..n)
                .filter_map(|i| bytes_of((self.api.module_name)(i)))
                .collect()
        }
    }

    fn lookup_uri(&self, uri: &[u8]) -> Option<Box<dyn SubRequest>> {
        if uri.contains(&0) {
            return None;
        }
        let c = cstring(uri);
        // SAFETY: `r` is live; the sub-request is destroyed on drop.
        let rr = unsafe { (self.api.sub_lookup)(self.r, c.as_ptr()) };
        if rr.is_null() {
            return None;
        }
        Some(Box::new(ApacheSubRequest {
            req: ApacheRequest {
                r: rr,
                api: self.api,
            },
        }))
    }

    fn top_request(&self) -> Option<Box<dyn Backend>> {
        // SAFETY: `r` is live; r->prev chain members outlive it.
        let top = unsafe { (self.api.top_request)(self.r) };
        if top.is_null() || top == self.r {
            return None;
        }
        Some(Box::new(ApacheRequest {
            r: top,
            api: self.api,
        }))
    }
}

/// A sub-request created by `ap_sub_req_lookup_uri`.
#[derive(Debug)]
struct ApacheSubRequest {
    req: ApacheRequest,
}

impl SubRequest for ApacheSubRequest {
    fn status(&self) -> i32 {
        i32::try_from(self.req.sint(IntField::Status as c_int)).unwrap_or(500)
    }

    fn info(&self) -> LookupInfo {
        use LookupValue::{Int, Str};
        let r = &self.req;
        let mut out: LookupInfo = Vec::new();
        let int = |f: IntField| Int(r.sint(f as c_int));
        let s = |out: &mut LookupInfo, name: &'static str, f: StrField| {
            if let Some(v) = r.sstr(f as c_int) {
                out.push((name, Str(v)));
            }
        };
        out.push(("status", int(IntField::Status)));
        s(&mut out, "the_request", StrField::TheRequest);
        s(&mut out, "status_line", StrField::StatusLine);
        s(&mut out, "method", StrField::Method);
        out.push(("mtime", Int(r.sint(IntField::Mtime as c_int) / 1_000_000)));
        out.push(("clength", int(IntField::Clength)));
        s(&mut out, "range", StrField::Range);
        out.push(("chunked", int(IntField::Chunked)));
        s(&mut out, "content_type", StrField::ContentType);
        s(&mut out, "handler", StrField::Handler);
        out.push(("no_cache", int(IntField::NoCache)));
        out.push(("no_local_copy", int(IntField::NoLocalCopy)));
        s(&mut out, "unparsed_uri", StrField::UnparsedUri);
        s(&mut out, "uri", StrField::Uri);
        s(&mut out, "filename", StrField::Filename);
        s(&mut out, "path_info", StrField::PathInfo);
        s(&mut out, "args", StrField::Args);
        out.push(("allowed", int(IntField::Allowed)));
        out.push(("sent_bodyct", int(IntField::SentBodyct)));
        out.push(("bytes_sent", int(IntField::BytesSent)));
        out.push(("mtime", int(IntField::Mtime)));
        out.push((
            "request_time",
            Int(r.sint(IntField::RequestTime as c_int) / 1_000_000),
        ));
        out
    }

    fn run(&self) -> i32 {
        // SAFETY: the sub-request is live until drop.
        unsafe { (self.req.api.sub_run)(self.req.r) }
    }
}

impl Drop for ApacheSubRequest {
    fn drop(&mut self) {
        // SAFETY: created by sub_lookup and destroyed exactly once.
        unsafe { (self.req.api.sub_destroy)(self.req.r) }
    }
}

/// Logs a server-level message (`ap_log_error`).
pub fn log_server(level: LogLevel, msg: &[u8]) {
    if let Some(api) = api() {
        let c = cstring(msg);
        // SAFETY: the message is logged with a "%s" format.
        unsafe { (api.log_server)(level as c_int, c.as_ptr()) }
    }
}

/// Called by the shim's `register_hooks`.
///
/// # Safety
/// `table` must point to the shim's static `phprs_api_table`.
#[no_mangle]
pub unsafe extern "C" fn phprust_set_api(table: *const ApiTable) {
    if table.is_null() {
        return;
    }
    // SAFETY: guaranteed by the caller.
    if unsafe { (*table).size } != std::mem::size_of::<ApiTable>() {
        return;
    }
    API.store(table.cast_mut(), Ordering::Release);
}

/// `post_config`: loads `php.ini` (once per configuration load).  Returns 0
/// on success.
///
/// # Safety
/// `ini_dir` must be null or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn phprust_post_config(ini_dir: *const c_char) -> c_int {
    // SAFETY: guaranteed by the caller.
    let dir = unsafe { bytes_of(ini_dir) };
    let r = std::panic::catch_unwind(move || {
        use std::os::unix::ffi::OsStrExt;
        let dir = dir.map(|d| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&d)));
        let (cfg, warnings) = crate::config::ProcessConfig::load(dir.as_deref());
        for w in warnings {
            log_server(LogLevel::Warning, format!("PHP Warning:  {w}").as_bytes());
        }
        crate::config::install(cfg);
    });
    c_int::from(r.is_err())
}

/// The `ap_add_version_component` string (`PHP/x.y.z`) when `expose_php`
/// is on, else null.
#[no_mangle]
pub extern "C" fn phprust_version_component() -> *const c_char {
    static COMPONENT: OnceLock<CString> = OnceLock::new();
    let cfg = crate::config::current();
    if !cfg.effective_bool("expose_php", &[], true) {
        return std::ptr::null();
    }
    COMPONENT
        .get_or_init(|| cstring(format!("PHP/{}", php_vm::PHP_VERSION).as_bytes()))
        .as_ptr()
}

/// `child_init`.
#[no_mangle]
pub extern "C" fn phprust_child_init() {
    crate::handler::child_init();
}

/// The content handler; returns `OK` (0), `DECLINED` (-1) or an HTTP
/// status.
///
/// # Safety
/// `r` must be the live `request_rec *` passed to the httpd handler hook.
#[no_mangle]
pub unsafe extern "C" fn phprust_handler(r: *mut c_void) -> c_int {
    // SAFETY: guaranteed by the caller for the duration of this call.
    let Some(req) = (unsafe { ApacheRequest::new(r) }) else {
        return -1;
    };
    let send = req.sendable();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::handler::handle(
            &req,
            Box::new(move || send.into_request().map(|r| Box::new(r) as Box<dyn Backend>)),
        )
    }));
    r.unwrap_or(500)
}
