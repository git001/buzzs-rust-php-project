// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! zlib's state goes through the request's counted heap, as php-src's
//! `php_zlib_alloc` (`safe_emalloc`) puts it under `memory_limit`.

use php_zlib::context::DeflateOptions;
use php_zlib::DeflateContext;

php_memlimit::install_global_allocator!();

const K: usize = 1024;
/// `ZLIB_ENCODING_DEFLATE`.
const ENCODING_DEFLATE: i64 = 15;

#[test]
fn deflate_state_is_counted() {
    php_memlimit::attach_thread();
    let before = php_memlimit::usage();
    let ctx = DeflateContext::new(ENCODING_DEFLATE, &DeflateOptions::default()).expect("init");
    // zlib's deflate state at windowBits 15, memLevel 8: window 64 KiB +
    // prev 64 KiB + head 64 KiB + pending 64 KiB (zlib's zlib.h: "(1 <<
    // (windowBits+2)) + (1 << (memLevel+9))").
    let grown = php_memlimit::usage() - before;
    assert!(grown > 200 * K, "deflate state not counted: {grown} bytes");
    drop(ctx);
    assert!(
        php_memlimit::usage() - before < 16 * K,
        "deflate state not released"
    );
}
