// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The C-library hooks (`php_memlimit::c_heap`): counted blocks, NULL past
//! the soft limit with the violation pending, fault injection.

use std::ffi::c_void;

use php_memlimit::c_heap;

php_memlimit::install_global_allocator!();

const M: usize = 1024 * 1024;

/// Runs `f` on a fresh thread with its own counter and a request under
/// `limit`.
fn request(limit: Option<usize>, f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        php_memlimit::attach_thread();
        php_memlimit::request_start(limit).expect("limit");
        f();
        php_memlimit::request_end();
    })
    .join()
    .expect("request thread");
}

#[test]
fn blocks_are_counted_and_released() {
    request(None, || {
        let before = php_memlimit::usage();
        // SAFETY: plain allocation calls, each block freed once.
        unsafe {
            let p = c_heap::malloc(M);
            assert!(!p.is_null());
            assert!(php_memlimit::usage() - before >= M);
            let q = c_heap::realloc(p, 3 * M);
            assert!(!q.is_null());
            assert!(php_memlimit::usage() - before >= 3 * M);
            let z = c_heap::calloc(1000, 1000).cast::<u8>();
            assert!(!z.is_null());
            assert_eq!(*z.add(999_999), 0);
            c_heap::free(q);
            c_heap::free(z.cast::<c_void>());
            let s = c_heap::strdup(c"abc".as_ptr());
            assert_eq!(std::ffi::CStr::from_ptr(s).to_bytes(), b"abc");
            c_heap::free(s.cast::<c_void>());
        }
        assert!(php_memlimit::usage() - before < 4096, "blocks not released");
    });
}

#[test]
fn null_past_the_soft_limit() {
    request(Some(16 * M), || {
        assert!(!php_memlimit::is_pending());
        // SAFETY: plain allocation calls; NULL is never freed or used.
        unsafe {
            assert!(c_heap::malloc(64 * M).is_null());
            let over = php_memlimit::take().expect("violation recorded");
            assert_eq!(over.tried, 64 * M);
            // Delivered once, as php-src's fatal; start over for the next case.
            php_memlimit::clear();

            // A failed realloc leaves the block alone.
            let p = c_heap::malloc(M).cast::<u8>();
            assert!(!p.is_null());
            *p = 7;
            assert!(c_heap::realloc(p.cast(), 64 * M).is_null());
            assert!(php_memlimit::is_pending());
            assert_eq!(*p, 7);
            c_heap::free(p.cast());

            assert!(c_heap::calloc(usize::MAX, 2).is_null());
        }
    });
}

#[test]
fn fault_injection_refuses_the_chosen_allocation() {
    request(None, || {
        php_memlimit::current().set_max_allocs(3);
        // SAFETY: plain allocation calls, each block freed once.
        unsafe {
            let a = c_heap::malloc(16);
            let b = c_heap::malloc(16);
            assert!(!a.is_null() && !b.is_null());
            assert!(c_heap::malloc(16).is_null(), "third allocation refused");
            assert!(php_memlimit::take().is_some());
            let d = c_heap::malloc(16);
            assert!(!d.is_null(), "later allocations granted");
            c_heap::free(a);
            c_heap::free(b);
            c_heap::free(d);
        }
    });
}
