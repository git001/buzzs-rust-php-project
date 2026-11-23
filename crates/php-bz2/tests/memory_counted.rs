// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Where libbz2's state is allocated follows php-src: the `bzip2.*` stream
//! filters use `php_bz2_alloc` (emalloc, counted against `memory_limit`),
//! `bzcompress()`/`bzdecompress()` leave libbz2 on malloc (not counted).

use php_bz2::filter::CompressFilter;
use php_bz2::{bzcompress, bzdecompress, Output};
use php_streams::filter::Seekable;

php_memlimit::install_global_allocator!();

const M: usize = 1024 * 1024;

#[test]
fn filter_state_is_counted() {
    php_memlimit::attach_thread();
    let before = php_memlimit::usage();
    let filter = CompressFilter::new(9, 0, Seekable::Never).expect("filter");
    // Block size 9 allocates about 7.6 MB of compressor state (bzip2
    // manual §2.2: 400k + 8 × 900k).
    let grown = php_memlimit::usage() - before;
    assert!(grown > 6 * M, "filter state not counted: {grown} bytes");
    drop(filter);
    assert!(
        php_memlimit::usage() - before < M,
        "filter state not released"
    );
}

#[test]
fn one_shot_state_is_not_counted() {
    php_memlimit::attach_thread();
    let data = vec![b'a'; M];
    php_memlimit::reset_peak();
    let before = php_memlimit::usage();
    let Output::Data(packed) = bzcompress(&data, 9, 0).expect("compress") else {
        panic!("bzcompress failed");
    };
    // Only the output buffer (input size + 1 %, + 600) counts.
    let grown = php_memlimit::peak() - before;
    assert!(grown < 2 * M, "bzcompress state counted: {grown} bytes");

    php_memlimit::reset_peak();
    let before = php_memlimit::usage();
    let Output::Data(back) = bzdecompress(&packed, false).expect("decompress") else {
        panic!("bzdecompress failed");
    };
    assert_eq!(back, data);
    // The output grows to 1 MiB (+ doubling slack); the ~3.6 MB state is not counted.
    let grown = php_memlimit::peak() - before;
    assert!(grown < 3 * M, "bzdecompress state counted: {grown} bytes");
}
