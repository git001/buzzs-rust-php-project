// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `malloc`-family hooks for C libraries whose memory php-src puts under
//! `memory_limit` (pcre2's match data: `php_pcre_emalloc`, a fallible
//! `femalloc`). Installed through the library's allocator hooks, they
//! count the library's memory on the calling thread's counter.
//!
//! The blocks themselves come from the C library's `malloc` (glibc); only
//! their usable size (`malloc_usable_size`) is counted. A block allocated
//! elsewhere and freed here, or the reverse, therefore only skews the
//! count, never the heap.
//!
//! A request past the soft limit (or the hard ceiling) gets NULL, with the
//! violation recorded, as php-src's fallible `femalloc` returns NULL and
//! sets the delayed OOM: the library reports out of memory and the VM
//! raises "Allowed memory size … exhausted" once the call returns. Only a
//! library that handles NULL cleanly may use these hooks (plan 3e, V4:
//! one fault-injection test per library); libxml2 does not (2.9.14 loops
//! in its HTML parser) and stays on plain malloc, as in php-src.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;

use crate::current;

/// Usable size of a block from these hooks (0 for NULL), for libraries
/// that ask a block's size (sqlite's `xSize`).
///
/// # Safety
///
/// `p` is NULL or a live block of the C allocator.
pub unsafe fn usable(p: *mut c_void) -> usize {
    if p.is_null() {
        return 0;
    }
    // SAFETY: the caller passes a live block.
    unsafe { libc::malloc_usable_size(p) }
}

/// `malloc(size)`, counted; NULL past the limit.
///
/// # Safety
///
/// As `malloc`.
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    let heap = current();
    if !heap.admits_c(size) {
        return ptr::null_mut();
    }
    // SAFETY: malloc has no preconditions.
    let p = unsafe { libc::malloc(size) };
    // SAFETY: `p` is NULL or the block malloc just returned.
    heap.account_c(unsafe { usable(p) }, 0);
    p
}

/// `calloc(count, size)`, counted; NULL past the limit or on overflow.
///
/// # Safety
///
/// As `calloc`.
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    let Some(bytes) = count.checked_mul(size) else {
        return ptr::null_mut();
    };
    let heap = current();
    if !heap.admits_c(bytes) {
        return ptr::null_mut();
    }
    // SAFETY: calloc has no preconditions.
    let p = unsafe { libc::calloc(count, size) };
    // SAFETY: `p` is NULL or the block malloc just returned.
    heap.account_c(unsafe { usable(p) }, 0);
    p
}

/// `realloc(p, size)`, counted; NULL (with `p` untouched) when growing
/// past the limit.
///
/// # Safety
///
/// As `realloc`: `p` is NULL or a live block of the C allocator.
pub unsafe extern "C" fn realloc(p: *mut c_void, size: usize) -> *mut c_void {
    let heap = current();
    // SAFETY: the caller passes NULL or a live block.
    let old = unsafe { usable(p) };
    if size > old && !heap.admits_c(size - old) {
        return ptr::null_mut();
    }
    // SAFETY: the caller passes NULL or a live block.
    let q = unsafe { libc::realloc(p, size) };
    if q.is_null() {
        // realloc(p, 0) may free p and return NULL.
        if size == 0 {
            heap.account_c(0, old);
        }
        return q;
    }
    // SAFETY: `q` is the block realloc just returned.
    heap.account_c(unsafe { usable(q) }, old);
    q
}

/// `free(p)`, counted.
///
/// # Safety
///
/// As `free`: `p` is NULL or a live block of the C allocator.
pub unsafe extern "C" fn free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    // SAFETY: the caller passes a live block.
    let size = unsafe { usable(p) };
    // SAFETY: the caller passes a live block.
    unsafe { libc::free(p) };
    current().account_c(0, size);
}

/// `strdup(s)` on the counted [`malloc`].
///
/// # Safety
///
/// `s` is a NUL-terminated string.
pub unsafe extern "C" fn strdup(s: *const c_char) -> *mut c_char {
    // SAFETY: the caller passes a NUL-terminated string.
    let bytes = unsafe { CStr::from_ptr(s) }.to_bytes_with_nul();
    // SAFETY: plain allocation.
    let p = unsafe { malloc(bytes.len()) }.cast::<u8>();
    if !p.is_null() {
        // SAFETY: `p` holds `bytes.len()` bytes and does not overlap `s`.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
    }
    p.cast()
}
