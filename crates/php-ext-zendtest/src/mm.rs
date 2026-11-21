// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `zend_mm_custom_handlers.c`: while
//! `zend_test.zend_mm_custom_handlers.enabled` is on, php-src runs the
//! request on an observed heap whose custom handlers report every
//! allocation, free, reallocation and garbage collection with `printf()`
//! (straight to stdout, outside PHP's output layer).
//!
//! Here the observed heap is [`php_memlimit`]'s heap observer: turning
//! the directive on makes the allocator report the request thread's heap
//! operations to [`observe`], which writes the same lines to file
//! descriptor 1. Shrinking reallocations are not reported (the allocator
//! keeps them on its fast path).

use php_memlimit::HeapEvent;
use php_vm::ext::{ModuleBuilder, VmResult};
use php_vm::Vm;
use std::cell::Cell;
use std::fmt;
use std::io::Write as _;

/// The directive (`STD_PHP_INI_BOOLEAN`, `PHP_INI_USER`).
pub(crate) const DIRECTIVE: &str = "zend_test.zend_mm_custom_handlers.enabled";

/// Stands in for the observed heap's address (`ZT_G(observed_heap)`).
static OBSERVED_HEAP: u8 = 0;

thread_local! {
    /// `ZT_G(observed_heap) != NULL`.
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// A pointer printed like glibc's `%p`: `0x…` in lowercase hex, `(nil)`
/// for null.
struct Ptr(usize);

impl fmt::Display for Ptr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            f.write_str("(nil)")
        } else {
            write!(f, "{:#x}", self.0)
        }
    }
}

/// `printf()` on the C stdout: written to file descriptor 1 directly,
/// since this runs inside the allocator, where neither Rust's locked
/// `Stdout` nor allocation is safe.
fn print_raw(mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: `bytes` is a live, initialised buffer of `bytes.len()`
        // bytes; `write(2)` only reads it and keeps no reference.
        let n = unsafe { libc::write(1, bytes.as_ptr().cast(), bytes.len()) };
        match usize::try_from(n) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
        }
    }
}

/// Formats `args` into a stack buffer (no allocation) and prints it.
fn printf(args: fmt::Arguments<'_>) {
    let mut buf = [0u8; 256];
    let mut cur = std::io::Cursor::new(&mut buf[..]);
    let _ = cur.write_fmt(args);
    let len = usize::try_from(cur.position()).unwrap_or(0);
    print_raw(buf.get(..len).unwrap_or_default());
}

/// The observed heap's handlers: `observe_malloc`, `observe_free`,
/// `observe_realloc` and `observe_gc`.
fn observe(event: HeapEvent) {
    match event {
        HeapEvent::Alloc { size, addr } => {
            printf(format_args!("Allocated {size} bytes at {}\n", Ptr(addr)));
        }
        HeapEvent::Free { addr } => printf(format_args!("Freed memory at {}\n", Ptr(addr))),
        HeapEvent::Realloc { size, old, new } => printf(format_args!(
            "Realloc of {size} bytes from {} to {}\n",
            Ptr(old),
            Ptr(new)
        )),
        HeapEvent::Gc { freed } => printf(format_args!("ZendMM GC freed {freed} bytes")),
    }
}

/// The original heap's address (`ZT_G(original_heap)`).
fn original_heap() -> usize {
    std::ptr::from_ref(php_memlimit::current()).addr()
}

/// `zend_test_mm_custom_handlers_init`.
fn init() {
    if INSTALLED.get() {
        return;
    }
    // The original heap has no custom handlers of its own.
    printf(format_args!(
        "Prev handlers at {0}, {0}, {0}, {0}, {0}\n",
        Ptr(0)
    ));
    INSTALLED.set(true);
    php_memlimit::observe_heap(true);
    printf(format_args!(
        "Heap at {} installed in ZendMM (orig at {})\n",
        Ptr(std::ptr::addr_of!(OBSERVED_HEAP).addr()),
        Ptr(original_heap())
    ));
}

/// `zend_test_mm_custom_handlers_shutdown`.
fn shutdown() {
    if !INSTALLED.get() {
        return;
    }
    php_memlimit::observe_heap(false);
    INSTALLED.set(false);
    printf(format_args!(
        "Prev heap at {} restored in ZendMM\n",
        Ptr(original_heap())
    ));
}

/// `OnUpdateZendTestMMCustomHandlersEnabled`.
fn on_modify(_vm: &mut Vm, value: &[u8]) -> VmResult<bool> {
    if php_support::ini::parse_bool(value) {
        init();
    } else {
        shutdown();
    }
    Ok(true)
}

/// `zend_test_mm_custom_handlers_rinit`.
fn request_startup(vm: &mut Vm) {
    if vm.ini_bool(DIRECTIVE) {
        init();
    }
}

/// `zend_test_mm_custom_handlers_rshutdown`.
fn request_shutdown(_vm: &mut Vm) {
    shutdown();
}

/// Installs the observer and the directive's hooks (the directive itself
/// is in [`crate::ini`]).
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    php_memlimit::set_heap_observer(observe);
    m.ini_on_modify(DIRECTIVE, on_modify);
    m.on_request_startup(request_startup);
    m.on_request_shutdown(request_shutdown);
}

#[cfg(test)]
mod tests {
    use super::Ptr;

    #[test]
    fn pointers_print_like_glibc() {
        assert_eq!(Ptr(0).to_string(), "(nil)");
        assert_eq!(Ptr(0x7f12_ab00).to_string(), "0x7f12ab00");
    }

    #[test]
    fn formatting_truncates_at_the_buffer() {
        // Longer than `printf`'s buffer: truncated, not a panic.
        let long = "x".repeat(1000);
        let mut buf = [0u8; 256];
        let mut cur = std::io::Cursor::new(&mut buf[..]);
        let _ = std::io::Write::write_fmt(&mut cur, format_args!("{long}"));
        assert_eq!(cur.position(), 256);
    }
}
