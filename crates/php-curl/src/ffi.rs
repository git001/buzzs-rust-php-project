// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Raw declarations of the libcurl 8.5.0 C API used by this crate.
//!
//! Only the subset needed by ext/curl is declared. Everything here is
//! `unsafe` to call; the safe wrappers live in [`crate::easy`],
//! [`crate::multi`], [`crate::share`] and [`crate::version`].

#![allow(
    non_camel_case_types,
    missing_docs,
    dead_code,
    clippy::upper_case_acronyms
)]

use std::os::raw::{c_char, c_double, c_int, c_long, c_uint, c_void};

/// `CURL` (opaque easy handle).
#[repr(C)]
pub struct CURL {
    _private: [u8; 0],
}
/// `CURLM` (opaque multi handle).
#[repr(C)]
pub struct CURLM {
    _private: [u8; 0],
}
/// `CURLSH` (opaque share handle).
#[repr(C)]
pub struct CURLSH {
    _private: [u8; 0],
}
/// `curl_mime` (opaque).
#[repr(C)]
pub struct curl_mime {
    _private: [u8; 0],
}
/// `curl_mimepart` (opaque).
#[repr(C)]
pub struct curl_mimepart {
    _private: [u8; 0],
}
/// `struct curl_pushheaders` (opaque).
#[repr(C)]
pub struct curl_pushheaders {
    _private: [u8; 0],
}

pub type CURLcode = c_int;
pub type CURLMcode = c_int;
pub type CURLSHcode = c_int;
pub type CURLoption = c_int;
pub type CURLINFO = c_int;
pub type curl_off_t = i64;

/// `struct curl_slist`.
#[repr(C)]
pub struct curl_slist {
    pub data: *mut c_char,
    pub next: *mut curl_slist,
}

/// `struct curl_certinfo`.
#[repr(C)]
pub struct curl_certinfo {
    pub num_of_certs: c_int,
    pub certinfo: *mut *mut curl_slist,
}

/// `struct curl_blob`.
#[repr(C)]
pub struct curl_blob {
    pub data: *mut c_void,
    pub len: usize,
    pub flags: c_uint,
}

/// `CURL_BLOB_COPY`.
pub const CURL_BLOB_COPY: c_uint = 1;

/// The `data` union of `CURLMsg`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union CURLMsgData {
    pub whatever: *mut c_void,
    pub result: CURLcode,
}

/// `struct CURLMsg`.
#[repr(C)]
pub struct CURLMsg {
    pub msg: c_int,
    pub easy_handle: *mut CURL,
    pub data: CURLMsgData,
}

/// `struct curl_easyoption`.
#[repr(C)]
pub struct curl_easyoption {
    pub name: *const c_char,
    pub id: CURLoption,
    pub kind: c_int,
    pub flags: c_uint,
}

/// `struct curl_waitfd`.
#[repr(C)]
pub struct curl_waitfd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

/// `curl_version_info_data` up to `CURLVERSION_ELEVENTH`.
#[repr(C)]
pub struct curl_version_info_data {
    pub age: c_int,
    pub version: *const c_char,
    pub version_num: c_uint,
    pub host: *const c_char,
    pub features: c_int,
    pub ssl_version: *const c_char,
    pub ssl_version_num: c_long,
    pub libz_version: *const c_char,
    pub protocols: *const *const c_char,
    pub ares: *const c_char,
    pub ares_num: c_int,
    pub libidn: *const c_char,
    pub iconv_ver_num: c_int,
    pub libssh_version: *const c_char,
    pub brotli_ver_num: c_uint,
    pub brotli_version: *const c_char,
    pub nghttp2_ver_num: c_uint,
    pub nghttp2_version: *const c_char,
    pub quic_version: *const c_char,
    pub cainfo: *const c_char,
    pub capath: *const c_char,
    pub zstd_ver_num: c_uint,
    pub zstd_version: *const c_char,
    pub hyper_version: *const c_char,
    pub gsasl_version: *const c_char,
    pub feature_names: *const *const c_char,
}

/// `CURLVERSION_NOW` of libcurl 8.5.0 (`CURLVERSION_ELEVENTH`).
pub const CURLVERSION_NOW: c_int = 10;
/// `CURL_GLOBAL_DEFAULT` (`CURL_GLOBAL_ALL`).
pub const CURL_GLOBAL_DEFAULT: c_long = 3;
/// `CURLMSG_DONE`.
pub const CURLMSG_DONE: c_int = 1;

pub type curl_write_callback = extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize;
pub type curl_read_callback = extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize;
pub type curl_seek_callback = extern "C" fn(*mut c_void, curl_off_t, c_int) -> c_int;
pub type curl_free_callback = extern "C" fn(*mut c_void);
pub type curl_progress_callback =
    extern "C" fn(*mut c_void, c_double, c_double, c_double, c_double) -> c_int;
pub type curl_xferinfo_callback =
    extern "C" fn(*mut c_void, curl_off_t, curl_off_t, curl_off_t, curl_off_t) -> c_int;
pub type curl_fnmatch_callback = extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
pub type curl_debug_callback =
    extern "C" fn(*mut CURL, c_int, *mut c_char, usize, *mut c_void) -> c_int;
pub type curl_prereq_callback =
    extern "C" fn(*mut c_void, *mut c_char, *mut c_char, c_int, c_int) -> c_int;
pub type curl_sshhostkey_callback =
    extern "C" fn(*mut c_void, c_int, *const c_char, usize) -> c_int;
pub type curl_push_callback =
    extern "C" fn(*mut CURL, *mut CURL, usize, *mut curl_pushheaders, *mut c_void) -> c_int;

// libcurl is bound lazily (see `build.rs`): the generated stubs define the
// functions below and load libcurl and its dependency tree (gnutls, libssh,
// libldap, ...) on the first call, keeping them out of every start-up.
include!(concat!(env!("OUT_DIR"), "/lazy_curl.rs"));

extern "C" {
    pub fn curl_global_init_mem(
        flags: c_long,
        malloc: unsafe extern "C" fn(usize) -> *mut c_void,
        free: unsafe extern "C" fn(*mut c_void),
        realloc: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void,
        strdup: unsafe extern "C" fn(*const c_char) -> *mut c_char,
        calloc: unsafe extern "C" fn(usize, usize) -> *mut c_void,
    ) -> CURLcode;
    pub fn curl_version_info(age: c_int) -> *const curl_version_info_data;

    pub fn curl_easy_init() -> *mut CURL;
    pub fn curl_easy_cleanup(h: *mut CURL);
    pub fn curl_easy_setopt(h: *mut CURL, option: CURLoption, ...) -> CURLcode;
    pub fn curl_easy_getinfo(h: *mut CURL, info: CURLINFO, ...) -> CURLcode;
    pub fn curl_easy_perform(h: *mut CURL) -> CURLcode;
    pub fn curl_easy_reset(h: *mut CURL);
    pub fn curl_easy_duphandle(h: *mut CURL) -> *mut CURL;
    pub fn curl_easy_strerror(code: CURLcode) -> *const c_char;
    pub fn curl_easy_escape(h: *mut CURL, s: *const c_char, len: c_int) -> *mut c_char;
    pub fn curl_easy_unescape(
        h: *mut CURL,
        s: *const c_char,
        len: c_int,
        outlen: *mut c_int,
    ) -> *mut c_char;
    pub fn curl_easy_pause(h: *mut CURL, bitmask: c_int) -> CURLcode;
    pub fn curl_easy_upkeep(h: *mut CURL) -> CURLcode;
    pub fn curl_easy_option_by_id(id: CURLoption) -> *const curl_easyoption;
    pub fn curl_free(p: *mut c_void);

    pub fn curl_slist_append(list: *mut curl_slist, s: *const c_char) -> *mut curl_slist;
    pub fn curl_slist_free_all(list: *mut curl_slist);

    pub fn curl_mime_init(h: *mut CURL) -> *mut curl_mime;
    pub fn curl_mime_free(m: *mut curl_mime);
    pub fn curl_mime_addpart(m: *mut curl_mime) -> *mut curl_mimepart;
    pub fn curl_mime_name(p: *mut curl_mimepart, name: *const c_char) -> CURLcode;
    pub fn curl_mime_filename(p: *mut curl_mimepart, name: *const c_char) -> CURLcode;
    pub fn curl_mime_type(p: *mut curl_mimepart, t: *const c_char) -> CURLcode;
    pub fn curl_mime_data(p: *mut curl_mimepart, data: *const c_char, size: usize) -> CURLcode;
    pub fn curl_mime_data_cb(
        p: *mut curl_mimepart,
        size: curl_off_t,
        readfunc: Option<curl_read_callback>,
        seekfunc: Option<curl_seek_callback>,
        freefunc: Option<curl_free_callback>,
        arg: *mut c_void,
    ) -> CURLcode;

    pub fn curl_multi_init() -> *mut CURLM;
    pub fn curl_multi_cleanup(m: *mut CURLM) -> CURLMcode;
    pub fn curl_multi_add_handle(m: *mut CURLM, e: *mut CURL) -> CURLMcode;
    pub fn curl_multi_remove_handle(m: *mut CURLM, e: *mut CURL) -> CURLMcode;
    pub fn curl_multi_perform(m: *mut CURLM, running: *mut c_int) -> CURLMcode;
    pub fn curl_multi_wait(
        m: *mut CURLM,
        extra: *mut curl_waitfd,
        extra_nfds: c_uint,
        timeout_ms: c_int,
        numfds: *mut c_int,
    ) -> CURLMcode;
    pub fn curl_multi_info_read(m: *mut CURLM, queued: *mut c_int) -> *mut CURLMsg;
    pub fn curl_multi_setopt(m: *mut CURLM, option: c_int, ...) -> CURLMcode;
    pub fn curl_multi_strerror(code: CURLMcode) -> *const c_char;
    pub fn curl_pushheader_bynum(h: *mut curl_pushheaders, num: usize) -> *mut c_char;

    pub fn curl_share_init() -> *mut CURLSH;
    pub fn curl_share_cleanup(s: *mut CURLSH) -> CURLSHcode;
    pub fn curl_share_setopt(s: *mut CURLSH, option: c_int, ...) -> CURLSHcode;
    pub fn curl_share_strerror(code: CURLSHcode) -> *const c_char;
}
