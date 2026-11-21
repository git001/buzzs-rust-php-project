// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Raw declarations for the parts of libmagic (`<magic.h>`, file 5.45) and
//! libc that this crate uses.  Everything here is `unsafe` to call; the
//! safe wrappers live in [`crate::magic`] and [`crate::capture`].

#![allow(non_camel_case_types, unsafe_code)]

use std::ffi::{c_char, c_int, c_void};

/// Opaque `struct magic_set`.
#[repr(C)]
pub struct magic_set {
    _private: [u8; 0],
}

/// `magic_t`.
pub type magic_t = *mut magic_set;

// libmagic is bound lazily (see `build.rs`): the generated stubs define the
// functions below and load the library on the first call, keeping it out of every
// start-up.
include!(concat!(env!("OUT_DIR"), "/lazy_magic.rs"));

unsafe extern "C" {
    pub fn magic_open(flags: c_int) -> magic_t;
    pub fn magic_close(ms: magic_t);
    pub fn magic_descriptor(ms: magic_t, fd: c_int) -> *const c_char;
    pub fn magic_buffer(ms: magic_t, buf: *const c_void, len: usize) -> *const c_char;
    pub fn magic_error(ms: magic_t) -> *const c_char;
    pub fn magic_getflags(ms: magic_t) -> c_int;
    pub fn magic_setflags(ms: magic_t, flags: c_int) -> c_int;
    pub fn magic_version() -> c_int;
    pub fn magic_load(ms: magic_t, path: *const c_char) -> c_int;
    pub fn magic_getpath(magicfile: *const c_char, action: c_int) -> *const c_char;
    pub fn magic_load_buffers(
        ms: magic_t,
        bufs: *mut *mut c_void,
        sizes: *mut usize,
        nbufs: usize,
    ) -> c_int;
    pub fn magic_errno(ms: magic_t) -> c_int;
    pub fn magic_getparam(ms: magic_t, param: c_int, value: *mut c_void) -> c_int;
    pub fn magic_setparam(ms: magic_t, param: c_int, value: *const c_void) -> c_int;
}

/// Opaque C `FILE`.
#[repr(C)]
pub struct CFile {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// The C library's `stderr` stream, which libmagic writes its
    /// warnings to.
    pub static mut stderr: *mut CFile;
    pub fn open_memstream(ptr: *mut *mut c_char, size: *mut usize) -> *mut CFile;
    pub fn fclose(stream: *mut CFile) -> c_int;
    pub fn free(ptr: *mut c_void);
}
