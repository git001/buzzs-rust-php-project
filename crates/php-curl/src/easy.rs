// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Easy handles (`CURL *`) with the bookkeeping ext/curl keeps next to
//! them: the error buffer, `curl_slist`s and MIME structures that must
//! outlive the handle (shared between duplicated handles), and the default
//! options of `_php_curl_set_default_options()`.

use crate::consts::{
    CURLE_OK, CURLE_UNKNOWN_OPTION, CURLINFO_PRIVATE, CURLOPT_COPYPOSTFIELDS,
    CURLOPT_DNS_CACHE_TIMEOUT, CURLOPT_ERRORBUFFER, CURLOPT_MAXREDIRS, CURLOPT_MIMEPOST,
    CURLOPT_NOPROGRESS, CURLOPT_POSTFIELDS, CURLOPT_POSTFIELDSIZE_LARGE, CURLOPT_PRIVATE,
    CURLOPT_PROTOCOLS, CURLOPT_SHARE, CURLOPT_STDERR, CURLOPT_VERBOSE, CURLPROTO_ALL,
    CURLPROTO_FILE, CURL_ERROR_SIZE, CURL_READFUNC_ABORT, CURL_SEEKFUNC_CANTSEEK, CURL_SEEKFUNC_OK,
};
use crate::ffi;
use crate::handler::{self, cint, Handler};
use crate::options::Callback;
use crate::share::Share;
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_void};
use std::rc::Rc;
use std::sync::Once;

/// `CURLE_BAD_FUNCTION_ARGUMENT`.
const CURLE_BAD_FUNCTION_ARGUMENT: i32 = 43;
/// `CURLE_OUT_OF_MEMORY`.
const CURLE_OUT_OF_MEMORY: i32 = 27;
/// Size of the error buffer (`CURL_ERROR_SIZE + 1`).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const ERRBUF_LEN: usize = CURL_ERROR_SIZE as usize + 1;

static GLOBAL_INIT: Once = Once::new();

/// `curl_global_init(CURL_GLOBAL_DEFAULT)` (MINIT), done once.
pub fn global_init() {
    GLOBAL_INIT.call_once(|| {
        // SAFETY: called once, before any other libcurl function, from a
        // single thread (the `Once` serialises callers).
        unsafe {
            ffi::curl_global_init(ffi::CURL_GLOBAL_DEFAULT);
        }
    });
}

thread_local! {
    static NEXT_KEY: Cell<u64> = const { Cell::new(1) };
}

fn fresh_key() -> u64 {
    NEXT_KEY.with(|k| {
        let v = k.get();
        k.set(v + 1);
        v
    })
}

/// A C option id from a PHP integer, `None` when out of range.
fn opt_id(opt: i64) -> Option<c_int> {
    c_int::try_from(opt).ok()
}

/// A file-backed (or otherwise streamed) MIME part body: the data source
/// behind a `CURLFile` upload.
pub trait MimeSource {
    /// Reads into `buf`; `None` aborts the transfer.
    fn read(&mut self, buf: &mut [u8]) -> Option<usize>;
    /// Seeks (`origin` is `SEEK_SET`/`SEEK_CUR`/`SEEK_END`); `false` when
    /// not possible.
    fn seek(&mut self, offset: i64, origin: i32) -> bool;
    /// Called when libcurl releases the part (closes the underlying file).
    fn close(&mut self);
}

/// One part of a `CURLOPT_POSTFIELDS` array.
pub enum MimePart {
    /// A plain `name=value` field.
    Data {
        /// Field name.
        name: Vec<u8>,
        /// Field value.
        data: Vec<u8>,
    },
    /// A `CURLFile` upload read through a [`MimeSource`].
    Stream {
        /// Field name.
        name: Vec<u8>,
        /// File name sent to the server.
        filename: Vec<u8>,
        /// Content type.
        mime_type: Vec<u8>,
        /// Size if known (`-1` otherwise).
        size: i64,
        /// Whether the source supports seeking.
        seekable: bool,
        /// The data source.
        source: Box<dyn MimeSource>,
    },
    /// A `CURLStringFile` upload.
    Buffer {
        /// Field name.
        name: Vec<u8>,
        /// File contents.
        data: Vec<u8>,
        /// File name sent to the server.
        filename: Vec<u8>,
        /// Content type.
        mime_type: Vec<u8>,
    },
}

impl std::fmt::Debug for MimePart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MimePart::Data { name, .. } => write!(f, "Data({})", String::from_utf8_lossy(name)),
            MimePart::Stream { name, .. } => write!(f, "Stream({})", String::from_utf8_lossy(name)),
            MimePart::Buffer { name, .. } => write!(f, "Buffer({})", String::from_utf8_lossy(name)),
        }
    }
}

/// Resources libcurl references by pointer (`struct _php_curl_free`),
/// shared by a handle and its duplicates and released after the last of
/// them is cleaned up.
#[derive(Default)]
struct Retained {
    slists: Vec<(i64, *mut ffi::curl_slist)>,
    mimes: Vec<*mut ffi::curl_mime>,
    sources: Vec<*mut Box<dyn MimeSource>>,
    files: Vec<*mut libc::FILE>,
}

impl Drop for Retained {
    fn drop(&mut self) {
        // SAFETY: every pointer was obtained from the matching libcurl/libc
        // allocator, is owned exclusively by this struct, and all easy
        // handles referencing them have been cleaned up (each `Easy` cleans
        // up its handle before releasing its `Rc<Retained>`). MIME
        // structures are freed before the sources their free callbacks use.
        unsafe {
            for m in self.mimes.drain(..) {
                ffi::curl_mime_free(m);
            }
            for s in self.sources.drain(..) {
                drop(Box::from_raw(s));
            }
            for (_, l) in self.slists.drain(..) {
                ffi::curl_slist_free_all(l);
            }
            for f in self.files.drain(..) {
                libc::fclose(f);
            }
        }
    }
}

/// The key of a raw handle (stored as `CURLOPT_PRIVATE`), if it is one of
/// ours.
pub(crate) fn key_of_raw(raw: *mut ffi::CURL) -> Option<u64> {
    if raw.is_null() {
        return None;
    }
    let mut p: *mut c_char = std::ptr::null_mut();
    // SAFETY: `raw` is a live easy handle given to us by libcurl and
    // CURLINFO_PRIVATE writes a `char *`.
    let rc = unsafe { ffi::curl_easy_getinfo(raw, cint(CURLINFO_PRIVATE), &raw mut p) };
    (rc == 0 && !p.is_null()).then_some(p as usize as u64)
}

/// A libcurl easy handle.
pub struct Easy {
    raw: Cell<*mut ffi::CURL>,
    key: u64,
    errbuf: *mut [u8; ERRBUF_LEN],
    retained: RefCell<Rc<RefCell<Retained>>>,
}

impl std::fmt::Debug for Easy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Easy")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl Drop for Easy {
    fn drop(&mut self) {
        let raw = self.raw.replace(std::ptr::null_mut());
        if !raw.is_null() {
            // SAFETY: `raw` is owned by this wrapper and not used afterwards.
            unsafe { ffi::curl_easy_cleanup(raw) };
        }
        // SAFETY: `errbuf` came from `Box::into_raw` and the handle that
        // wrote into it is gone.
        drop(unsafe { Box::from_raw(self.errbuf) });
    }
}

fn new_errbuf() -> *mut [u8; ERRBUF_LEN] {
    Box::into_raw(Box::new([0u8; ERRBUF_LEN]))
}

impl Easy {
    /// `curl_easy_init()` plus ext/curl's default options; `cainfo` is
    /// `openssl.cafile`/`curl.cainfo` when set.
    pub fn new(cainfo: Option<&[u8]>) -> Option<Easy> {
        global_init();
        // SAFETY: plain constructor call.
        let raw = unsafe { ffi::curl_easy_init() };
        if raw.is_null() {
            return None;
        }
        let e = Easy {
            raw: Cell::new(raw),
            key: fresh_key(),
            errbuf: new_errbuf(),
            retained: RefCell::new(Rc::default()),
        };
        e.set_defaults(cainfo);
        Some(e)
    }

    /// Wraps a handle created by libcurl for a server push.
    pub(crate) fn adopt(raw: *mut ffi::CURL) -> Option<Easy> {
        if raw.is_null() {
            return None;
        }
        let e = Easy {
            raw: Cell::new(raw),
            key: fresh_key(),
            errbuf: new_errbuf(),
            retained: RefCell::new(Rc::default()),
        };
        e.rebind();
        Some(e)
    }

    /// Forgets the handle without cleaning it up (a denied server push:
    /// libcurl frees it).
    pub fn disown(&self) {
        self.raw.set(std::ptr::null_mut());
    }

    /// Shares the retained resources of `other` (a pushed handle inherits
    /// its parent's lists).
    pub fn share_retained_with(&self, other: &Easy) {
        *self.retained.borrow_mut() = Rc::clone(&other.retained.borrow());
    }

    /// The key identifying this handle in [`Handler`] callbacks.
    pub fn key(&self) -> u64 {
        self.key
    }

    pub(crate) fn raw(&self) -> *mut ffi::CURL {
        self.raw.get()
    }

    #[allow(clippy::cast_possible_truncation)]
    fn ud(&self) -> *mut c_void {
        self.key as usize as *mut c_void
    }

    fn setopt_long_raw(&self, opt: i64, v: c_long) -> i32 {
        let (raw, Some(o)) = (self.raw(), opt_id(opt)) else {
            return cint(CURLE_UNKNOWN_OPTION);
        };
        if raw.is_null() {
            return CURLE_BAD_FUNCTION_ARGUMENT;
        }
        // SAFETY: live handle; long-typed options take a `long` vararg
        // (for other option types libcurl rejects or ignores the value
        // according to the option's own type, as in C callers).
        unsafe { ffi::curl_easy_setopt(raw, o, v) }
    }

    fn setopt_ptr_raw(&self, opt: i64, v: *const c_void) -> i32 {
        let (raw, Some(o)) = (self.raw(), opt_id(opt)) else {
            return cint(CURLE_UNKNOWN_OPTION);
        };
        if raw.is_null() {
            return CURLE_BAD_FUNCTION_ARGUMENT;
        }
        // SAFETY: live handle; object/function-pointer options take a
        // pointer-sized vararg. Callers keep pointed-to data alive as long
        // as libcurl may use it (see `Retained`).
        unsafe { ffi::curl_easy_setopt(raw, o, v) }
    }

    fn setopt_off_raw(&self, opt: i64, v: i64) -> i32 {
        let (raw, Some(o)) = (self.raw(), opt_id(opt)) else {
            return cint(CURLE_UNKNOWN_OPTION);
        };
        if raw.is_null() {
            return CURLE_BAD_FUNCTION_ARGUMENT;
        }
        // SAFETY: live handle; `curl_off_t` options take an `i64` vararg.
        unsafe { ffi::curl_easy_setopt(raw, o, v) }
    }

    /// Points the callback user data and the error buffer at this wrapper
    /// (after `curl_easy_duphandle()` or a push).
    fn rebind(&self) {
        self.setopt_ptr_raw(CURLOPT_ERRORBUFFER, self.errbuf.cast::<c_void>());
        for cb in Callback::ALL {
            self.setopt_ptr_raw(cb.data_option(), self.ud());
        }
        self.setopt_ptr_raw(CURLOPT_PRIVATE, self.ud());
    }

    /// `_php_curl_set_default_options()`.
    fn set_defaults(&self, cainfo: Option<&[u8]>) {
        self.setopt_long_raw(CURLOPT_NOPROGRESS, 1);
        self.setopt_long_raw(CURLOPT_VERBOSE, 0);
        self.setopt_ptr_raw(CURLOPT_ERRORBUFFER, self.errbuf.cast::<c_void>());
        self.set_callback(Callback::Write, true);
        self.set_callback(Callback::Read, true);
        self.set_callback(Callback::Header, true);
        self.setopt_long_raw(CURLOPT_DNS_CACHE_TIMEOUT, 120);
        self.setopt_long_raw(CURLOPT_MAXREDIRS, 20);
        if let Some(ca) = cainfo.filter(|c| !c.is_empty()) {
            self.set_str(crate::consts::CURLOPT_CAINFO, ca);
        }
        self.setopt_ptr_raw(CURLOPT_PRIVATE, self.ud());
    }

    /// Installs (or, with `on == false`, removes) the trampoline of a
    /// callback and points its user data at this handle.
    pub fn set_callback(&self, cb: Callback, on: bool) {
        let f: *const c_void = if on {
            match cb {
                Callback::Write => handler::write_cb as *const c_void,
                Callback::Header => handler::header_cb as *const c_void,
                Callback::Read => handler::read_cb as *const c_void,
                Callback::Seek => handler::seek_cb as *const c_void,
                Callback::Progress => handler::progress_cb as *const c_void,
                Callback::XferInfo => handler::xferinfo_cb as *const c_void,
                Callback::Fnmatch => handler::fnmatch_cb as *const c_void,
                Callback::Debug => handler::debug_cb as *const c_void,
                Callback::Prereq => handler::prereq_cb as *const c_void,
                Callback::SshHostKey => handler::sshhostkey_cb as *const c_void,
            }
        } else {
            std::ptr::null()
        };
        self.setopt_ptr_raw(cb.function_option(), f);
        let data = if on { self.ud() } else { std::ptr::null_mut() };
        self.setopt_ptr_raw(cb.data_option(), data);
    }

    /// `CURLINFO_HEADER_OUT` as an option: debug trampoline + verbose.
    pub fn set_header_out(&self, on: bool) {
        self.set_callback(Callback::Debug, on);
        self.setopt_long_raw(CURLOPT_VERBOSE, c_long::from(on));
    }

    /// A long option.
    pub fn set_long(&self, opt: i64, v: i64) -> i32 {
        self.setopt_long_raw(opt, v)
    }

    /// A `curl_off_t` option.
    pub fn set_off_t(&self, opt: i64, v: i64) -> i32 {
        self.setopt_off_raw(opt, v)
    }

    /// A string option (libcurl copies it). The caller rejects NUL bytes.
    pub fn set_str(&self, opt: i64, v: &[u8]) -> i32 {
        match CString::new(v) {
            Ok(c) => self.setopt_ptr_raw(opt, c.as_ptr().cast::<c_void>()),
            Err(_) => CURLE_BAD_FUNCTION_ARGUMENT,
        }
    }

    /// Resets a pointer option to `NULL`.
    pub fn set_null(&self, opt: i64) -> i32 {
        self.setopt_ptr_raw(opt, std::ptr::null())
    }

    /// A `struct curl_blob` option (copied by libcurl).
    pub fn set_blob(&self, opt: i64, data: &[u8]) -> i32 {
        let mut blob = ffi::curl_blob {
            data: data.as_ptr().cast_mut().cast::<c_void>(),
            len: data.len(),
            flags: ffi::CURL_BLOB_COPY,
        };
        self.setopt_ptr_raw(opt, std::ptr::addr_of_mut!(blob).cast::<c_void>())
    }

    /// A `curl_slist` option built from `items` (the caller rejects NUL
    /// bytes). `None` when the list could not be built.
    pub fn set_slist(&self, opt: i64, items: &[Vec<u8>]) -> Option<i32> {
        let mut list: *mut ffi::curl_slist = std::ptr::null_mut();
        for it in items {
            let Ok(c) = CString::new(it.as_slice()) else {
                // SAFETY: `list` was built by curl_slist_append.
                unsafe { ffi::curl_slist_free_all(list) };
                return None;
            };
            // SAFETY: `list` is null or a list built here; `c` is valid.
            let n = unsafe { ffi::curl_slist_append(list, c.as_ptr()) };
            if n.is_null() {
                // SAFETY: as above.
                unsafe { ffi::curl_slist_free_all(list) };
                return None;
            }
            list = n;
        }
        if !list.is_null() {
            let shared = Rc::clone(&self.retained.borrow());
            let sole = Rc::strong_count(&shared) == 2; // `shared` + ours
            let mut r = shared.borrow_mut();
            if sole {
                if let Some(slot) = r.slists.iter_mut().find(|(o, _)| *o == opt) {
                    let old = std::mem::replace(&mut slot.1, list);
                    drop(r);
                    let rc = self.setopt_ptr_raw(opt, list.cast::<c_void>());
                    // SAFETY: the handle no longer references `old`.
                    unsafe { ffi::curl_slist_free_all(old) };
                    return Some(rc);
                }
            }
            r.slists.push((opt, list));
        }
        Some(self.setopt_ptr_raw(opt, list.cast::<c_void>()))
    }

    /// `CURLOPT_POSTFIELDS` with a string (size first, then
    /// `CURLOPT_COPYPOSTFIELDS`).
    pub fn set_postfields(&self, data: &[u8]) -> i32 {
        let len = i64::try_from(data.len()).unwrap_or(i64::MAX);
        self.setopt_off_raw(CURLOPT_POSTFIELDSIZE_LARGE, len);
        let p: *const c_void = if data.is_empty() {
            c"".as_ptr().cast::<c_void>()
        } else {
            data.as_ptr().cast::<c_void>()
        };
        self.setopt_ptr_raw(CURLOPT_COPYPOSTFIELDS, p)
    }

    /// `CURLOPT_POSTFIELDS` with an empty array.
    pub fn set_empty_postfields(&self) -> i32 {
        static EMPTY: &[u8] = b"\0";
        self.setopt_ptr_raw(CURLOPT_POSTFIELDS, EMPTY.as_ptr().cast::<c_void>());
        self.setopt_off_raw(CURLOPT_POSTFIELDSIZE_LARGE, 0)
    }

    /// Builds a MIME structure from `parts` and sets it as
    /// `CURLOPT_MIMEPOST`. Returns the libcurl error code.
    pub fn set_mime(&self, parts: Vec<MimePart>) -> i32 {
        let raw = self.raw();
        if raw.is_null() {
            return CURLE_BAD_FUNCTION_ARGUMENT;
        }
        let mime = if parts.is_empty() {
            std::ptr::null_mut()
        } else {
            // SAFETY: live handle.
            let m = unsafe { ffi::curl_mime_init(raw) };
            if m.is_null() {
                return CURLE_OUT_OF_MEMORY;
            }
            m
        };
        let shared = Rc::clone(&self.retained.borrow());
        for part in parts {
            let rc = add_part(mime, part, &shared);
            if rc != 0 {
                // SAFETY: `mime` was created above and is not referenced by
                // the handle yet.
                unsafe { ffi::curl_mime_free(mime) };
                return rc;
            }
        }
        let sole = Rc::strong_count(&shared) == 2;
        let old: Vec<*mut ffi::curl_mime> = if sole {
            std::mem::take(&mut shared.borrow_mut().mimes)
        } else {
            Vec::new()
        };
        if !mime.is_null() {
            shared.borrow_mut().mimes.push(mime);
        }
        let rc = self.setopt_ptr_raw(CURLOPT_MIMEPOST, mime.cast::<c_void>());
        for m in old {
            // SAFETY: only this handle used `m` and it now points at `mime`.
            unsafe { ffi::curl_mime_free(m) };
        }
        rc
    }

    /// `CURLOPT_STDERR`: a duplicate of `fd` opened for writing, or the
    /// process stderr for `None`.
    pub fn set_stderr(&self, fd: Option<i32>) -> i32 {
        let fp = match fd {
            None => std::ptr::null_mut(),
            Some(fd) => {
                // SAFETY: `dup` on a descriptor the caller owns; the copy is
                // owned by the FILE created next.
                let d = unsafe { libc::dup(fd) };
                if d < 0 {
                    return CURLE_BAD_FUNCTION_ARGUMENT;
                }
                // SAFETY: `d` is a fresh descriptor, the mode is a valid C
                // string.
                let fp = unsafe { libc::fdopen(d, c"a".as_ptr()) };
                if fp.is_null() {
                    // SAFETY: `d` is ours.
                    unsafe { libc::close(d) };
                    return CURLE_BAD_FUNCTION_ARGUMENT;
                }
                self.retained.borrow().borrow_mut().files.push(fp);
                fp
            }
        };
        self.setopt_ptr_raw(CURLOPT_STDERR, fp.cast::<c_void>())
    }

    /// Flushes libc buffers of the `CURLOPT_STDERR` files.
    pub fn flush_stderr(&self) {
        for &f in &self.retained.borrow().borrow().files {
            // SAFETY: `f` is an open FILE owned by `Retained`.
            unsafe { libc::fflush(f) };
        }
    }

    /// `CURLOPT_SHARE`.
    pub fn set_share(&self, sh: &Share) -> i32 {
        self.setopt_ptr_raw(CURLOPT_SHARE, sh.raw().cast::<c_void>())
    }

    /// Disables `file://` (`open_basedir` in effect).
    pub fn disable_file_protocol(&self) {
        self.setopt_long_raw(CURLOPT_PROTOCOLS, CURLPROTO_ALL & !CURLPROTO_FILE);
    }

    /// `curl_easy_perform()` with `h` receiving the callbacks.
    pub fn perform(&self, h: &mut dyn Handler) -> i32 {
        let raw = self.raw();
        if raw.is_null() {
            return CURLE_BAD_FUNCTION_ARGUMENT;
        }
        // SAFETY: live handle; callbacks reach `h` through the handler slot.
        handler::with_handler(h, || unsafe { ffi::curl_easy_perform(raw) })
    }

    /// `curl_easy_reset()` followed by the default options.
    pub fn reset(&self, cainfo: Option<&[u8]>) {
        let raw = self.raw();
        if raw.is_null() {
            return;
        }
        // SAFETY: live handle.
        unsafe { ffi::curl_easy_reset(raw) };
        self.set_defaults(cainfo);
    }

    /// `curl_easy_duphandle()`: the copy shares the retained lists.
    pub fn duplicate(&self) -> Option<Easy> {
        let raw = self.raw();
        if raw.is_null() {
            return None;
        }
        // SAFETY: live handle.
        let dup = unsafe { ffi::curl_easy_duphandle(raw) };
        if dup.is_null() {
            return None;
        }
        let e = Easy {
            raw: Cell::new(dup),
            key: fresh_key(),
            errbuf: new_errbuf(),
            retained: RefCell::new(Rc::clone(&self.retained.borrow())),
        };
        e.rebind();
        Some(e)
    }

    /// The error buffer contents (up to the first NUL).
    pub fn error_message(&self) -> Vec<u8> {
        // SAFETY: `errbuf` is a live allocation owned by this wrapper; no
        // transfer is writing into it while we read (single thread).
        let buf = unsafe { &*self.errbuf };
        let end = buf.iter().position(|&b| b == 0).unwrap_or(ERRBUF_LEN - 1);
        buf[..end].to_vec()
    }

    /// Clears the error buffer.
    pub fn clear_error(&self) {
        // SAFETY: see `error_message`.
        unsafe { (*self.errbuf).fill(0) };
    }

    fn getinfo<T: Default>(&self, info: i64) -> Option<T> {
        let raw = self.raw();
        let o = opt_id(info)?;
        if raw.is_null() {
            return None;
        }
        let mut v = T::default();
        // SAFETY: live handle; callers pick `T` matching the info's type
        // class (long / double / curl_off_t / pointer).
        let rc = unsafe { ffi::curl_easy_getinfo(raw, o, &raw mut v) };
        (rc == 0).then_some(v)
    }

    /// A `CURLINFO_LONG` value.
    pub fn info_long(&self, info: i64) -> Option<i64> {
        self.getinfo::<c_long>(info)
    }

    /// A `CURLINFO_DOUBLE` value.
    pub fn info_double(&self, info: i64) -> Option<f64> {
        self.getinfo::<f64>(info)
    }

    /// A `CURLINFO_OFF_T` value.
    pub fn info_off_t(&self, info: i64) -> Option<i64> {
        self.getinfo::<i64>(info)
    }

    /// A `CURLINFO_STRING` value: `Some(None)` when libcurl returned NULL.
    pub fn info_str(&self, info: i64) -> Option<Option<Vec<u8>>> {
        let raw = self.raw();
        let o = opt_id(info)?;
        if raw.is_null() {
            return None;
        }
        let mut p: *const c_char = std::ptr::null();
        // SAFETY: live handle; string infos write a `char *`.
        let rc = unsafe { ffi::curl_easy_getinfo(raw, o, &raw mut p) };
        if rc != 0 {
            return None;
        }
        Some((!p.is_null()).then(|| {
            // SAFETY: libcurl returns a NUL-terminated string valid until
            // the next call on the handle.
            unsafe { CStr::from_ptr(p) }.to_bytes().to_vec()
        }))
    }

    /// A `CURLINFO_SLIST` value (the list is freed afterwards).
    pub fn info_slist(&self, info: i64) -> Option<Vec<Vec<u8>>> {
        let raw = self.raw();
        let o = opt_id(info)?;
        if raw.is_null() {
            return None;
        }
        let mut p: *mut ffi::curl_slist = std::ptr::null_mut();
        // SAFETY: live handle; slist infos write a `struct curl_slist *`.
        let rc = unsafe { ffi::curl_easy_getinfo(raw, o, &raw mut p) };
        if rc != 0 {
            return None;
        }
        let out = slist_items(p);
        // SAFETY: the caller owns lists returned by these infos.
        unsafe { ffi::curl_slist_free_all(p) };
        Some(out)
    }

    /// `CURLINFO_CERTINFO`: per certificate, its `key:value` lines.
    pub fn info_certinfo(&self) -> Option<Vec<Vec<Vec<u8>>>> {
        let raw = self.raw();
        if raw.is_null() {
            return None;
        }
        let mut p: *mut ffi::curl_certinfo = std::ptr::null_mut();
        // SAFETY: live handle; CURLINFO_CERTINFO writes a pointer to a
        // struct owned by the handle.
        let rc = unsafe {
            ffi::curl_easy_getinfo(raw, cint(crate::consts::CURLINFO_CERTINFO), &raw mut p)
        };
        if rc != 0 {
            return None;
        }
        let mut out = Vec::new();
        if !p.is_null() {
            // SAFETY: `p` points at the handle's certinfo struct.
            let ci = unsafe { &*p };
            for i in 0..usize::try_from(ci.num_of_certs).unwrap_or(0) {
                // SAFETY: `certinfo` has `num_of_certs` entries.
                let l = unsafe { *ci.certinfo.add(i) };
                out.push(slist_items(l));
            }
        }
        Some(out)
    }

    /// `curl_easy_escape()`.
    pub fn escape(&self, s: &[u8]) -> Option<Vec<u8>> {
        let len = c_int::try_from(s.len()).ok()?;
        let raw = self.raw();
        // SAFETY: `s` is valid for `len` bytes; the result is freed with
        // curl_free.
        unsafe {
            let r = ffi::curl_easy_escape(raw, s.as_ptr().cast::<c_char>(), len);
            if r.is_null() {
                return None;
            }
            let out = CStr::from_ptr(r).to_bytes().to_vec();
            ffi::curl_free(r.cast::<c_void>());
            Some(out)
        }
    }

    /// `curl_easy_unescape()`.
    pub fn unescape(&self, s: &[u8]) -> Option<Vec<u8>> {
        let len = c_int::try_from(s.len()).ok()?;
        let raw = self.raw();
        let mut out_len: c_int = 0;
        // SAFETY: as in `escape`; `out_len` receives the decoded length.
        unsafe {
            let r =
                ffi::curl_easy_unescape(raw, s.as_ptr().cast::<c_char>(), len, &raw mut out_len);
            if r.is_null() {
                return None;
            }
            let n = usize::try_from(out_len).unwrap_or(0);
            let out = std::slice::from_raw_parts(r.cast::<u8>(), n).to_vec();
            ffi::curl_free(r.cast::<c_void>());
            Some(out)
        }
    }

    /// `curl_easy_pause()`; unpausing may run callbacks, which reach `h`.
    pub fn pause(&self, bitmask: i64, h: &mut dyn Handler) -> i32 {
        let raw = self.raw();
        if raw.is_null() {
            return CURLE_BAD_FUNCTION_ARGUMENT;
        }
        // SAFETY: live handle. The mask is truncated like the C `int`
        // parameter.
        #[allow(clippy::cast_possible_truncation)]
        handler::with_handler(h, || unsafe { ffi::curl_easy_pause(raw, bitmask as c_int) })
    }

    /// `curl_easy_upkeep()`.
    pub fn upkeep(&self) -> i32 {
        let raw = self.raw();
        if raw.is_null() {
            return CURLE_BAD_FUNCTION_ARGUMENT;
        }
        // SAFETY: live handle.
        unsafe { ffi::curl_easy_upkeep(raw) }
    }
}

/// The strings of a `curl_slist`.
fn slist_items(mut p: *const ffi::curl_slist) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while !p.is_null() {
        // SAFETY: `p` is a node of a valid list.
        let node = unsafe { &*p };
        if !node.data.is_null() {
            // SAFETY: node data is a NUL-terminated string.
            out.push(unsafe { CStr::from_ptr(node.data) }.to_bytes().to_vec());
        }
        p = node.next;
    }
    out
}

/// A C string truncated at the first NUL (what `ZSTR_VAL` gives C code).
fn c_trunc(s: &[u8]) -> CString {
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    CString::new(&s[..end]).unwrap_or_default()
}

extern "C" fn mime_read_cb(
    buf: *mut c_char,
    size: usize,
    nitems: usize,
    arg: *mut c_void,
) -> usize {
    let len = size.saturating_mul(nitems);
    // SAFETY: `arg` is the `Box<dyn MimeSource>` registered with this part,
    // kept alive by `Retained` until after the MIME structure is freed.
    let src = unsafe { &mut *arg.cast::<Box<dyn MimeSource>>() };
    if len == 0 || buf.is_null() {
        return 0;
    }
    // SAFETY: libcurl hands over a writable buffer of `len` bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), len) };
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    src.read(out).unwrap_or(CURL_READFUNC_ABORT as usize)
}

extern "C" fn mime_seek_cb(arg: *mut c_void, offset: ffi::curl_off_t, origin: c_int) -> c_int {
    // SAFETY: see `mime_read_cb`.
    let src = unsafe { &mut *arg.cast::<Box<dyn MimeSource>>() };
    if src.seek(offset, origin) {
        cint(CURL_SEEKFUNC_OK)
    } else {
        cint(CURL_SEEKFUNC_CANTSEEK)
    }
}

extern "C" fn mime_free_cb(arg: *mut c_void) {
    // SAFETY: see `mime_read_cb`.
    let src = unsafe { &mut *arg.cast::<Box<dyn MimeSource>>() };
    src.close();
}

/// Adds one part to `mime`; returns a libcurl error code.
fn add_part(mime: *mut ffi::curl_mime, part: MimePart, shared: &Rc<RefCell<Retained>>) -> i32 {
    // SAFETY: `mime` is a live MIME structure owned by the caller.
    let p = unsafe { ffi::curl_mime_addpart(mime) };
    if p.is_null() {
        return CURLE_OUT_OF_MEMORY;
    }
    let chain = |steps: &[&dyn Fn() -> i32]| -> i32 {
        for s in steps {
            let rc = s();
            if rc != 0 {
                return rc;
            }
        }
        0
    };
    // SAFETY (all closures below): `p` is a live part of `mime`; strings are
    // valid C strings and data slices are copied by libcurl.
    match part {
        MimePart::Data { name, data } => {
            let n = c_trunc(&name);
            chain(&[
                &|| unsafe { ffi::curl_mime_name(p, n.as_ptr()) },
                &|| unsafe { ffi::curl_mime_data(p, data.as_ptr().cast::<c_char>(), data.len()) },
            ])
        }
        MimePart::Buffer {
            name,
            data,
            filename,
            mime_type,
        } => {
            let (n, f, t) = (c_trunc(&name), c_trunc(&filename), c_trunc(&mime_type));
            chain(&[
                &|| unsafe { ffi::curl_mime_name(p, n.as_ptr()) },
                &|| unsafe { ffi::curl_mime_data(p, data.as_ptr().cast::<c_char>(), data.len()) },
                &|| unsafe { ffi::curl_mime_filename(p, f.as_ptr()) },
                &|| unsafe { ffi::curl_mime_type(p, t.as_ptr()) },
            ])
        }
        MimePart::Stream {
            name,
            filename,
            mime_type,
            size,
            seekable,
            source,
        } => {
            let arg: *mut Box<dyn MimeSource> = Box::into_raw(Box::new(source));
            shared.borrow_mut().sources.push(arg);
            let (n, f, t) = (c_trunc(&name), c_trunc(&filename), c_trunc(&mime_type));
            let seek: Option<ffi::curl_seek_callback> =
                if seekable { Some(mime_seek_cb) } else { None };
            chain(&[
                &|| unsafe { ffi::curl_mime_name(p, n.as_ptr()) },
                &|| unsafe {
                    ffi::curl_mime_data_cb(
                        p,
                        size,
                        Some(mime_read_cb),
                        seek,
                        Some(mime_free_cb),
                        arg.cast::<c_void>(),
                    )
                },
                &|| unsafe { ffi::curl_mime_filename(p, f.as_ptr()) },
                &|| unsafe { ffi::curl_mime_type(p, t.as_ptr()) },
            ])
        }
    }
}

/// `curl_easy_strerror()`.
pub fn strerror(code: i64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY: curl_easy_strerror accepts any value and returns a static
    // string.
    let p = unsafe { ffi::curl_easy_strerror(code as c_int) };
    if p.is_null() {
        return String::new();
    }
    // SAFETY: static NUL-terminated string.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// `curl_easy_option_by_id(opt)->name` (`HTTPHEADER` for
/// `CURLOPT_HTTPHEADER`).
pub fn option_name(opt: i64) -> Option<String> {
    let o = opt_id(opt)?;
    // SAFETY: lookup in libcurl's static option table.
    let p = unsafe { ffi::curl_easy_option_by_id(o) };
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` points at a static table entry with a valid name.
    let name = unsafe { (*p).name };
    if name.is_null() {
        return None;
    }
    // SAFETY: static NUL-terminated string.
    Some(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Checks `CURLE_OK`.
pub fn is_ok(code: i32) -> bool {
    i64::from(code) == CURLE_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{
        CURLINFO_EFFECTIVE_URL, CURLINFO_HTTP_CODE, CURLOPT_HTTPHEADER, CURLOPT_URL,
    };

    #[test]
    fn create_and_info() {
        let e = Easy::new(None).expect("easy");
        assert_eq!(e.set_str(CURLOPT_URL, b"http://example.invalid/x"), 0);
        assert_eq!(e.info_long(CURLINFO_HTTP_CODE), Some(0));
        assert_eq!(
            e.info_str(CURLINFO_EFFECTIVE_URL),
            Some(Some(b"http://example.invalid/x".to_vec()))
        );
        assert_eq!(key_of_raw(e.raw()), Some(e.key()));
        let d = e.duplicate().expect("dup");
        assert_ne!(d.key(), e.key());
        assert_eq!(key_of_raw(d.raw()), Some(d.key()));
        assert_eq!(
            e.set_slist(CURLOPT_HTTPHEADER, &[b"A: b".to_vec()]),
            Some(0)
        );
    }

    #[test]
    fn escape_roundtrip() {
        let e = Easy::new(None).expect("easy");
        assert_eq!(e.escape(b"a b&c").as_deref(), Some(&b"a%20b%26c"[..]));
        assert_eq!(e.unescape(b"a%20b%00c").as_deref(), Some(&b"a b\0c"[..]));
    }

    #[test]
    fn names_and_errors() {
        assert_eq!(
            option_name(CURLOPT_HTTPHEADER).as_deref(),
            Some("HTTPHEADER")
        );
        assert_eq!(strerror(0), "No error");
        assert_eq!(strerror(1), "Unsupported protocol");
    }

    #[test]
    fn file_url_transfer() {
        struct Collect(Vec<u8>);
        impl Handler for Collect {
            fn write(&mut self, _key: u64, data: &[u8]) -> usize {
                self.0.extend_from_slice(data);
                data.len()
            }
        }
        let dir = std::env::temp_dir().join(format!("php-curl-test-{}", std::process::id()));
        std::fs::write(&dir, b"hello").expect("write");
        let e = Easy::new(None).expect("easy");
        let url = format!("file://{}", dir.display());
        assert_eq!(e.set_str(CURLOPT_URL, url.as_bytes()), 0);
        let mut c = Collect(Vec::new());
        assert_eq!(e.perform(&mut c), 0);
        assert_eq!(c.0, b"hello");
        let _ = std::fs::remove_file(dir);
    }
}
